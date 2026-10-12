use super::*;
use std::sync::atomic::AtomicUsize;
use tinybus::module::abi::TbSlice;
unsafe extern "C" fn send(_: *mut c_void, _: *const u8, _: usize) -> i32 { TB_OK }
unsafe extern "C" fn notify(_: *mut c_void) {}
unsafe extern "C" fn fault(_: *mut c_void, _: *const u8, _: usize) {}
unsafe extern "C" fn log(_: *mut c_void, _: u32, _: *const u8, _: usize) {}
fn host() -> TbHostVtable {
    TbHostVtable { size: size_of::<TbHostVtable>() as u32, _reserved: 0,
        host_ctx: std::ptr::null_mut(), send, wake: notify, log, ready: notify, fault,
        config: TbSlice { ptr: std::ptr::null(), len: 0 }, broker_routing: None, native_terminal_state: None }
}
fn output() -> TbModuleVtable {
    TbModuleVtable { size: size_of::<TbModuleVtable>() as u32, _reserved: 0,
        module_ctx: std::ptr::null_mut(), deliver, shutdown, reinitialize: None }
}
#[test]
fn native_query_is_required_before_terminal_factory_evaluation() {
    let calls = AtomicUsize::new(0);
    let code = unsafe { start_terminal_reconfigurable_module_with_factory::<serde_json::Value, _, _>(
        &host(), &mut output(), 1, true, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((|_: Connection, _: serde_json::Value| async { Ok(()) },
                RuntimeHooks::new(|_| {}, || TerminalCompletion::PhysicallyStopped)))
        }) };
    assert_eq!(code, TB_CLOSED);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "missing native query must leave the factory untouched");
}

unsafe extern "C" fn native_query(ctx: *mut c_void) -> u32 {
    unsafe { &*ctx.cast::<std::sync::atomic::AtomicU32>() }.load(Ordering::Acquire)
}
#[test]
fn shutdown_rechecks_actual_native_fault_after_local_stop_and_physical_finish() {
    let flags = Arc::new(std::sync::atomic::AtomicU32::new(tinybus::module::abi::TB_NATIVE_TERMINAL_LIVE));
    let mut host = host();
    host.host_ctx = Arc::as_ptr(&flags).cast_mut().cast();
    host.native_terminal_state = Some(native_query);
    let state = terminal::TerminalState::admit(HostCalls(host), None).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let worker_release = release.clone();
    state.register(RuntimeHooks::new(|_| {}, move || {
        entered_tx.send(()).unwrap(); worker_release.wait(); TerminalCompletion::PhysicallyStopped
    }));
    state.mark_vtable_published(); state.publish();
    let ptr = state.context().addr();
    let caller = std::thread::spawn(move || unsafe { terminal_shutdown(ptr as *mut c_void, 3000) });
    entered_rx.recv().unwrap();
    flags.store(tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT, Ordering::Release);
    release.wait();
    let code = caller.join().unwrap();
    assert_eq!(code, TB_PANICKED, "actual native Fault cannot disappear behind local Stop");
    drop(state);
    assert!(terminal::TerminalState::lookup(std::ptr::with_exposed_provenance_mut(ptr)).is_some(), "published Fault must retain the exact anchor instead of refunding its permit");
}

mod controlled {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    #[derive(serde::Deserialize)]
    pub struct Config { pub mode: u8 }
    #[derive(Debug)]
    pub enum Event { Poll(u8), Request, FutureDrop(String), Finish, FactoryDrop(String) }
    pub struct Fixture {
        pub events: std::sync::mpsc::Sender<Event>,
        pub view: StdMutex<Option<NativeInstanceLiveness>>,
        wake: (StdMutex<bool>, std::sync::Condvar),
        worker: StdMutex<Option<std::thread::JoinHandle<bool>>>,
        pub requests: AtomicUsize, pub joins: AtomicUsize,
    }
    static NEXT: StdMutex<Option<Arc<Fixture>>> = StdMutex::new(None);
    pub static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    pub fn fixture() -> (Arc<Fixture>, std::sync::mpsc::Receiver<Event>) {
        let (events, receiver) = std::sync::mpsc::channel();
        let fixture = Arc::new(Fixture { events, view: StdMutex::new(None),
            wake: (StdMutex::new(false), std::sync::Condvar::new()),
            worker: StdMutex::new(None), requests: AtomicUsize::new(0), joins: AtomicUsize::new(0) });
        *NEXT.lock().unwrap() = Some(fixture.clone());
        (fixture, receiver)
    }
    struct Capture(Arc<Fixture>);
    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = self.0.events.send(Event::FactoryDrop(std::thread::current().name().unwrap_or("unnamed").to_owned()));
        }
    }
    struct SetupFuture { fixture: Arc<Fixture>, mode: u8, first: bool }
    impl Future for SetupFuture {
        type Output = Result<()>;
        fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
            if self.first { self.first = false; self.fixture.events.send(Event::Poll(self.mode)).unwrap(); }
            match self.mode { 0 => Poll::Ready(Ok(())), 2 => panic!("controlled setup poll"),
                3 => Poll::Ready(Err(Error::failed("controlled setup failure"))), _ => Poll::Pending }
        }
    }
    impl Drop for SetupFuture {
        fn drop(&mut self) {
            let _ = self.fixture.events.send(Event::FutureDrop(std::thread::current().name().unwrap_or("unnamed").to_owned()));
        }
    }
    fn factory(view: NativeInstanceLiveness) -> Result<(impl Fn(Connection, Config) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync, RuntimeHooks)> {
        let fixture = NEXT.lock().unwrap().take().unwrap();
        *fixture.view.lock().unwrap() = Some(view.clone());
        let (request, finish) = (fixture.clone(), fixture.clone());
        let capture = Capture(fixture.clone());
        let setup = move |_: Connection, config: Config| {
            let _ = &capture;
            let mut worker = fixture.worker.lock().unwrap();
            if worker.is_none() {
                let work = fixture.clone(); let worker_view = view.clone();
                *worker = Some(std::thread::Builder::new().name("controlled-module-worker".into()).spawn(move || {
                    let mut cancelled = work.wake.0.lock().unwrap();
                    while !*cancelled { cancelled = work.wake.1.wait(cancelled).unwrap(); }
                    drop(cancelled);
                    !worker_view.is_live()
                }).unwrap());
            }
            drop(worker);
            if config.mode == 4 { panic!("controlled setup construction"); }
            Box::pin(SetupFuture { fixture: fixture.clone(), mode: config.mode, first: true }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
        };
        let hooks = RuntimeHooks::new(move |_| {
            request.requests.fetch_add(1, Ordering::SeqCst);
            *request.wake.0.lock().unwrap() = true; request.wake.1.notify_all();
            request.events.send(Event::Request).unwrap();
        }, move || {
            let worker = finish.worker.lock().unwrap().take();
            if let Some(worker) = worker {
                let denied = worker.join().unwrap();
                finish.joins.fetch_add(1, Ordering::SeqCst);
                if !denied { return TerminalCompletion::Uncertain; }
            }
            finish.events.send(Event::Finish).unwrap();
            TerminalCompletion::PhysicallyStopped
        });
        Ok((setup, hooks))
    }
    module_export_static! {
        terminal_setup_factory = factory, config = Config, worker_threads = 1,
        provides = ["ai.tinyhumans.tinybus.Terminal"], methods = [], signals = [],
        requires = [], optional = [], lazy = false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_initial_cancellation_drops_future_and_joins_worker_on_physical_thread() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker.clone().spawn(tinybus::transport::memory::MemoryBus::new());
    let host = tinybus::module::ModuleHost::new(broker);
    host.attach_linked_with_config(controlled::linked_module().unwrap(), serde_json::json!({"mode":1})).unwrap();
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    let shutdown = host.shutdown(Duration::from_secs(5));
    shutdown.await;
    let remainder: Vec<_> = events.try_iter().collect();
    let request_count = fixture.requests.load(Ordering::SeqCst);
    let joins = fixture.joins.load(Ordering::SeqCst);
    // All actual workers/futures settled before asserting behavior.
    assert!(matches!(entered, controlled::Event::Poll(1)));
    assert_eq!(request_count, 1); assert_eq!(joins, 1);
    assert!(!view.is_live());
    let future_drop = remainder.iter().position(|event| matches!(event, controlled::Event::FutureDrop(name) if name == "tinybus-terminal")).unwrap();
    let finish = remainder.iter().position(|event| matches!(event, controlled::Event::Finish)).unwrap();
    assert!(future_drop < finish);
    assert!(remainder.iter().any(|event| matches!(event, controlled::Event::FactoryDrop(name) if name == "tinybus-terminal")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_setup_construction_and_poll_panics_still_finish_real_worker_once() {
    let _serial = controlled::TEST_LOCK.lock().await;
    for mode in [2, 4] {
        let (fixture, events) = controlled::fixture();
        let broker = tinybus::broker::Broker::new();
        broker.clone().spawn(tinybus::transport::memory::MemoryBus::new());
        let host = tinybus::module::ModuleHost::new(broker);
        host.attach_linked_with_config(controlled::linked_module().unwrap(), serde_json::json!({"mode":mode})).unwrap();
        // Consume through actual join notification before assertions or host teardown.
        let mut seen = Vec::new();
        loop {
            let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
            let finished = matches!(event, controlled::Event::Finish); seen.push(event);
            if finished { break; }
        }
        host.shutdown(Duration::from_secs(5)).await;
        let view = fixture.view.lock().unwrap().take().unwrap();
        assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
        assert!(!view.is_live());
        if mode == 2 {
            assert!(seen.iter().any(|event| matches!(event, controlled::Event::FutureDrop(name) if name == "tinybus-terminal")));
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeControls { ctx: *mut c_void, event: unsafe extern "C" fn(*mut c_void, u32, u32) }
unsafe extern "C" fn native_event(ctx: *mut c_void, event: u32, value: u32) {
    let sender = unsafe { &*ctx.cast::<std::sync::mpsc::Sender<(u32, u32)>>() };
    let _ = sender.send((event, value));
}
// Only fixture symbols are looked up; TinyBus itself verifies/adopts the pinned
// private artifact through the actual ModuleHost loader first. It remains mapped.
#[cfg(unix)]
unsafe fn terminal_fixture_symbols(path: &std::path::Path) -> (extern "C" fn() -> usize, extern "C" fn(usize) -> i32) {
    use std::os::unix::ffi::OsStrExt;
    #[cfg_attr(target_os = "linux", link(name = "dl"))]
    unsafe extern "C" { fn dlopen(path: *const std::ffi::c_char, flags: i32) -> *mut c_void; fn dlsym(handle: *mut c_void, name: *const std::ffi::c_char) -> *mut c_void; }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let handle = unsafe { dlopen(path.as_ptr(), 2) };
    assert!(!handle.is_null());
    let context = unsafe { dlsym(handle, c"terminal_test_context".as_ptr()) };
    let probe = unsafe { dlsym(handle, c"terminal_test_old_context".as_ptr()) };
    assert!(!context.is_null() && !probe.is_null());
    unsafe { (std::mem::transmute::<*mut c_void, extern "C" fn() -> usize>(context), std::mem::transmute::<*mut c_void, extern "C" fn(usize) -> i32>(probe)) }
}
#[cfg(windows)]
unsafe fn terminal_fixture_symbols(path: &std::path::Path) -> (extern "C" fn() -> usize, extern "C" fn(usize) -> i32) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" { fn LoadLibraryW(path: *const u16) -> *mut c_void; fn GetProcAddress(handle: *mut c_void, name: *const u8) -> *mut c_void; }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe { LoadLibraryW(path.as_ptr()) };
    assert!(!handle.is_null());
    let context = unsafe { GetProcAddress(handle, c"terminal_test_context".as_ptr().cast()) };
    let probe = unsafe { GetProcAddress(handle, c"terminal_test_old_context".as_ptr().cast()) };
    assert!(!context.is_null() && !probe.is_null());
    unsafe { (std::mem::transmute::<*mut c_void, extern "C" fn() -> usize>(context), std::mem::transmute::<*mut c_void, extern "C" fn(usize) -> i32>(probe)) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a privately pinned TINYBUS_TEST_TERMINAL_MODULE compiled cdylib"]
async fn compiled_native_terminal_owns_initial_cancellation_panics_join_and_stale_context() {
    let path = std::path::PathBuf::from(std::env::var_os("TINYBUS_TEST_TERMINAL_MODULE").unwrap());
    let digest = tinybus::module::sha256_file(&path).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    let pins = std::fs::read_to_string(path.parent().unwrap().join("modules.toml")).unwrap();
    assert!(pins.contains(&format!("\"{name}\" = \"{digest}\"")));
    let mut old_context = None;
    for mode in [1u8, 0, 2, 4] {
        let (events, receiver) = std::sync::mpsc::channel::<(u32, u32)>();
        let events = Box::new(events);
        let controls = NativeControls { ctx: (&*events as *const std::sync::mpsc::Sender<(u32, u32)>).cast_mut().cast(), event: native_event };
        let encoded = unsafe { std::slice::from_raw_parts((&controls as *const NativeControls).cast::<u8>(), size_of::<NativeControls>()) }.to_vec();
        let broker = tinybus::broker::Broker::new();
        broker.clone().spawn(tinybus::transport::memory::MemoryBus::new());
        let host = tinybus::module::ModuleHost::new(broker);
        host.load_file_with_config(&path, serde_json::json!({"mode":mode,"test_controls":encoded})).unwrap();
        let (context, old_probe) = unsafe { terminal_fixture_symbols(&path) };
        let actual_context = context();
        let first = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let stale_result = old_context.map(old_probe);
        host.shutdown(Duration::from_secs(5)).await;
        let mut seen = vec![first];
        while !seen.iter().any(|(event, _)| *event == 6) { seen.push(receiver.recv_timeout(Duration::from_secs(5)).unwrap()); }
        // All actual controller workers and SDK captures are destroyed before assertions.
        if let Some(code) = stale_result { assert_eq!(code, TB_CLOSED); }
        assert_eq!(seen.iter().filter(|(event, _)| *event == 3).count(), 1);
        assert_eq!(seen.iter().filter(|(event, value)| *event == 5 && *value == 1).count(), 1);
        assert_eq!(seen.iter().filter(|(event, value)| *event == 7 && *value == 1).count(), 1);
        assert!(seen.iter().any(|(event, value)| *event == 6 && *value == 1));
        if mode != 4 {
            let drop = seen.iter().position(|(event, value)| *event == 4 && *value == 1).unwrap();
            let finish = seen.iter().position(|(event, _)| *event == 5).unwrap();
            assert!(drop < finish);
        }
        old_context = Some(actual_context);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_reinit_terminal_cancellation_settles_before_worker_join() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker.clone().spawn(tinybus::transport::memory::MemoryBus::new());
    let host = tinybus::module::ModuleHost::new(broker);
    host.attach_linked_with_config(controlled::linked_module().unwrap(), serde_json::json!({"mode":0})).unwrap();
    let initial = events.recv_timeout(Duration::from_secs(3)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !host.list().iter().any(|info| matches!(info.state, tinybus::module::ModuleState::Ready)) { tokio::task::yield_now().await; }
    }).await.unwrap();
    let reinit_host = host.clone();
    let reinit = tokio::spawn(async move { reinit_host.reinitialize("tinybus-module", serde_json::json!({"mode":1})).await });
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    view.request_fault();
    let reinit_result = reinit.await.unwrap();
    host.shutdown(Duration::from_secs(5)).await;
    let remainder: Vec<_> = events.try_iter().collect();
    assert!(matches!(initial, controlled::Event::Poll(0)));
    assert!(matches!(entered, controlled::Event::Poll(1)));
    assert!(reinit_result.is_err());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
    assert!(!view.is_live());
    let drops: Vec<_> = remainder.iter().enumerate().filter_map(|(index, event)| matches!(event, controlled::Event::FutureDrop(name) if name == "tinybus-terminal").then_some(index)).collect();
    let finish = remainder.iter().position(|event| matches!(event, controlled::Event::Finish)).unwrap();
    assert_eq!(drops.len(), 2);
    assert!(drops.into_iter().all(|index| index < finish));
}
