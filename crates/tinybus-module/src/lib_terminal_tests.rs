use super::*;
use std::sync::atomic::AtomicUsize;
use tinybus::module::abi::TbSlice;
unsafe extern "C" fn send(_: *mut c_void, _: *const u8, _: usize) -> i32 {
    TB_OK
}
unsafe extern "C" fn notify(_: *mut c_void) {}
unsafe extern "C" fn fault(_: *mut c_void, _: *const u8, _: usize) {}
unsafe extern "C" fn log(_: *mut c_void, _: u32, _: *const u8, _: usize) {}
fn host() -> TbHostVtable {
    TbHostVtable {
        size: size_of::<TbHostVtable>() as u32,
        _reserved: 0,
        host_ctx: std::ptr::null_mut(),
        send,
        wake: notify,
        log,
        ready: notify,
        fault,
        config: TbSlice {
            ptr: std::ptr::null(),
            len: 0,
        },
        broker_routing: None,
        native_terminal_state: None,
    }
}
fn output() -> TbModuleVtable {
    TbModuleVtable {
        size: size_of::<TbModuleVtable>() as u32,
        _reserved: 0,
        module_ctx: std::ptr::null_mut(),
        deliver,
        shutdown,
        reinitialize: None,
    }
}
#[test]
fn native_query_is_required_before_terminal_factory_evaluation() {
    use tinybus::module::abi::*;
    // Exactly LIVE is the sole admitted value. Missing, empty, terminal,
    // combined and unknown flags must not evaluate arbitrary factory code.
    for flags in [
        None,
        Some(0),
        Some(TB_NATIVE_TERMINAL_STOP_INTENT),
        Some(TB_NATIVE_TERMINAL_FAULT),
        Some(TB_NATIVE_TERMINAL_PHYSICALLY_STOPPED),
        Some(TB_NATIVE_TERMINAL_LIVE | TB_NATIVE_TERMINAL_STOP_INTENT),
        Some(TB_NATIVE_TERMINAL_LIVE | TB_NATIVE_TERMINAL_FAULT),
        Some(TB_NATIVE_TERMINAL_LIVE | (1 << 31)),
        Some(u32::MAX),
    ] {
        let native = std::sync::atomic::AtomicU32::new(flags.unwrap_or(0));
        let mut host = host();
        host.host_ctx = (&native as *const std::sync::atomic::AtomicU32).cast_mut().cast();
        host.native_terminal_state = flags.map(|_| native_query as unsafe extern "C" fn(*mut c_void) -> u32);
        let calls = AtomicUsize::new(0);
        let mut out = output();
        let code = unsafe {
            start_terminal_reconfigurable_module_with_factory::<serde_json::Value, _, _>(
                &host,
                &mut out,
                1,
                true,
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok((
                        |_: Connection, _: serde_json::Value| async { Ok(()) },
                        RuntimeHooks::new(|_| {}, || TerminalCompletion::PhysicallyStopped),
                    ))
                },
            )
        };
        assert_eq!(code, TB_CLOSED, "query {flags:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "query {flags:?}");
        assert!(out.module_ctx.is_null(), "denied init cannot publish output");
    }
}

#[test]
fn invalid_initial_config_rejects_before_factory_or_output_publication() {
    let native = std::sync::atomic::AtomicU32::new(tinybus::module::abi::TB_NATIVE_TERMINAL_LIVE);
    let bytes = b"{";
    let mut host = host();
    host.host_ctx = (&native as *const std::sync::atomic::AtomicU32).cast_mut().cast();
    host.native_terminal_state = Some(native_query);
    host.config = TbSlice { ptr: bytes.as_ptr(), len: bytes.len() };
    let calls = AtomicUsize::new(0);
    let mut out = output();
    let code = unsafe {
        start_terminal_reconfigurable_module_with_factory::<serde_json::Value, _, _>(
            &host, &mut out, 1, true,
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((
                    |_: Connection, _: serde_json::Value| async { Ok(()) },
                    RuntimeHooks::new(|_| {}, || TerminalCompletion::PhysicallyStopped),
                ))
            },
        )
    };
    assert_eq!(code, TB_BAD_ARGUMENT);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(out.module_ctx.is_null());
}

unsafe extern "C" fn native_query(ctx: *mut c_void) -> u32 {
    unsafe { &*ctx.cast::<std::sync::atomic::AtomicU32>() }.load(Ordering::Acquire)
}
#[test]
fn shutdown_rechecks_actual_native_fault_after_local_stop_and_physical_finish() {
    static FLAGS: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(tinybus::module::abi::TB_NATIVE_TERMINAL_LIVE);
    let flags = &FLAGS;
    let mut host = host();
    host.host_ctx = (flags as *const std::sync::atomic::AtomicU32)
        .cast_mut()
        .cast();
    host.native_terminal_state = Some(native_query);
    let state = terminal::TerminalState::admit(HostCalls(host), None).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let worker_release = release.clone();
    state.register(RuntimeHooks::new(
        |_| {},
        move || {
            entered_tx.send(()).unwrap();
            worker_release.wait();
            TerminalCompletion::PhysicallyStopped
        },
    ));
    state.mark_vtable_published();
    state.publish();
    let ptr = state.context().addr();
    let caller = std::thread::spawn(move || unsafe { terminal_shutdown(ptr as *mut c_void, 3000) });
    entered_rx.recv().unwrap();
    flags.store(
        tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT,
        Ordering::Release,
    );
    release.wait();
    let code = caller.join().unwrap();
    assert_eq!(
        code, TB_PANICKED,
        "actual native Fault cannot disappear behind local Stop"
    );
    drop(state);
    assert!(
        terminal::TerminalState::lookup(std::ptr::with_exposed_provenance_mut(ptr)).is_some(),
        "published Fault must retain the exact anchor instead of refunding its permit"
    );
}

mod controlled {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    #[derive(serde::Deserialize)]
    pub struct Config {
        pub mode: u8,
        #[serde(default, deserialize_with = "parse_gate")]
        #[serde(rename = "parse_gate")]
        _parse_gate: bool,
        #[serde(default)]
        record_drop: bool,
        #[serde(skip, default = "config_fixture")]
        fixture: std::sync::Weak<Fixture>,
    }
    static CONFIG_FIXTURE: StdMutex<std::sync::Weak<Fixture>> =
        StdMutex::new(std::sync::Weak::new());
    fn config_fixture() -> std::sync::Weak<Fixture> {
        CONFIG_FIXTURE.lock().unwrap().clone()
    }
    fn parse_gate<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<bool, D::Error> {
        let enabled = <bool as serde::Deserialize>::deserialize(deserializer)?;
        if enabled {
            let fixture = config_fixture().upgrade().unwrap();
            let barrier = fixture.config_parse_control.lock().unwrap().take().unwrap();
            fixture.events.send(Event::ConfigParsing).unwrap();
            barrier.wait();
        }
        Ok(enabled)
    }
    impl Drop for Config {
        fn drop(&mut self) {
            if self.record_drop {
                let Some(fixture) = self.fixture.upgrade() else {
                    return;
                };
                fixture.config_drops.fetch_add(1, Ordering::SeqCst);
                let _ = fixture.events.send(Event::ConfigDrop(
                    std::thread::current()
                        .name()
                        .unwrap_or("unnamed")
                        .to_owned(),
                ));
            }
        }
    }
    #[derive(Debug)]
    pub enum Event {
        Poll(u8),
        Request,
        FutureDrop(String),
        ConfigDrop(String),
        ConfigParsing,
        TimerConstruct(std::thread::ThreadId, String, tokio::runtime::Id),
        TimerDrop(Option<tokio::runtime::Id>),
        Finish,
        FactoryDrop(String),
    }
    pub struct Fixture {
        pub events: std::sync::mpsc::Sender<Event>,
        pub view: StdMutex<Option<NativeInstanceLiveness>>,
        wake: (StdMutex<bool>, std::sync::Condvar),
        worker: StdMutex<Option<std::thread::JoinHandle<bool>>>,
        pub requests: AtomicUsize,
        pub joins: AtomicUsize,
        pub finishes: AtomicUsize,
        pub hook_failure: AtomicUsize,
        pub payload_drops: AtomicUsize,
        pub config_drops: AtomicUsize,
        pub config_parse_control: StdMutex<Option<Arc<std::sync::Barrier>>>,
        pub drop_off_lock: AtomicBool,
        pub request_off_lock: AtomicBool,
        pub future_drop_control: StdMutex<Option<(Arc<std::sync::Barrier>, bool)>>,
    }
    static NEXT: StdMutex<Option<Arc<Fixture>>> = StdMutex::new(None);
    pub static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    pub fn fixture() -> (Arc<Fixture>, std::sync::mpsc::Receiver<Event>) {
        let (events, receiver) = std::sync::mpsc::channel();
        let fixture = Arc::new(Fixture {
            events,
            view: StdMutex::new(None),
            wake: (StdMutex::new(false), std::sync::Condvar::new()),
            worker: StdMutex::new(None),
            requests: AtomicUsize::new(0),
            joins: AtomicUsize::new(0),
            finishes: AtomicUsize::new(0),
            hook_failure: AtomicUsize::new(0),
            payload_drops: AtomicUsize::new(0),
            config_drops: AtomicUsize::new(0),
            config_parse_control: StdMutex::new(None),
            drop_off_lock: AtomicBool::new(true),
            request_off_lock: AtomicBool::new(true),
            future_drop_control: StdMutex::new(None),
        });
        *CONFIG_FIXTURE.lock().unwrap() = Arc::downgrade(&fixture);
        *NEXT.lock().unwrap() = Some(fixture.clone());
        (fixture, receiver)
    }
    struct HookPayload(Arc<Fixture>);
    impl Drop for HookPayload {
        fn drop(&mut self) {
            self.0.payload_drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Fixture {
        pub fn settle_rejected_worker(&self) {
            *self.wake.0.lock().unwrap() = true;
            self.wake.1.notify_all();
            if let Some(worker) = self.worker.lock().unwrap().take() {
                let _ = worker.join().unwrap();
                self.joins.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    struct Capture(Arc<Fixture>);
    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = self.0.events.send(Event::FactoryDrop(
                std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_owned(),
            ));
        }
    }
    struct SetupFuture {
        fixture: Arc<Fixture>,
        mode: u8,
        first: bool,
        timer: Option<Pin<Box<tokio::time::Sleep>>>,
    }
    impl Future for SetupFuture {
        type Output = Result<()>;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            if self.first {
                self.first = false;
                self.fixture.events.send(Event::Poll(self.mode)).unwrap();
            }
            match self.mode {
                0 => Poll::Ready(Ok(())),
                5 => self
                    .timer
                    .as_mut()
                    .unwrap()
                    .as_mut()
                    .poll(cx)
                    .map(|()| Ok(())),
                2 => panic!("controlled setup poll"),
                3 => Poll::Ready(Err(Error::failed("controlled setup failure"))),
                _ => Poll::Pending,
            }
        }
    }
    impl Drop for SetupFuture {
        fn drop(&mut self) {
            if self.mode == 5 {
                let _ = self.fixture.events.send(Event::TimerDrop(
                    tokio::runtime::Handle::try_current()
                        .ok()
                        .map(|handle| handle.id()),
                ));
            }
            if let Some(view) = self.fixture.view.lock().unwrap().as_ref() {
                let slot = view.inner.resources.lock().unwrap().reinitialize.clone();
                if slot.try_lock().is_err() {
                    self.fixture.drop_off_lock.store(false, Ordering::Release);
                }
            }
            let _ = self.fixture.events.send(Event::FutureDrop(
                std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_owned(),
            ));
            let control = self.fixture.future_drop_control.lock().unwrap().take();
            if let Some((release, panic)) = control {
                release.wait();
                if panic {
                    panic!("controlled prior reinit future Drop");
                }
            }
        }
    }
    type FixtureSetup = Box<dyn Fn(Connection, Config) -> terminal::SetupFuture + Send + Sync>;
    fn factory(view: NativeInstanceLiveness) -> Result<(FixtureSetup, RuntimeHooks)> {
        let fixture = NEXT.lock().unwrap().take().unwrap();
        *fixture.view.lock().unwrap() = Some(view.clone());
        let (request, finish) = (fixture.clone(), fixture.clone());
        let capture = Capture(fixture.clone());
        let setup = move |_: Connection, config: Config| {
            let _ = &capture;
            let mut worker = fixture.worker.lock().unwrap();
            if worker.is_none() {
                let work = fixture.clone();
                let worker_view = view.clone();
                *worker = Some(
                    std::thread::Builder::new()
                        .name("controlled-module-worker".into())
                        .spawn(move || {
                            let mut cancelled = work.wake.0.lock().unwrap();
                            while !*cancelled {
                                cancelled = work.wake.1.wait(cancelled).unwrap();
                            }
                            drop(cancelled);
                            !worker_view.is_live()
                        })
                        .unwrap(),
                );
            }
            drop(worker);
            if config.mode == 4 {
                panic!("controlled setup construction");
            }
            let timer = (config.mode == 5).then(|| {
                // Construct synchronously in F, before returning its future.
                let timer = Box::pin(tokio::time::sleep(Duration::from_millis(1)));
                fixture
                    .events
                    .send(Event::TimerConstruct(
                        std::thread::current().id(),
                        std::thread::current()
                            .name()
                            .unwrap_or("unnamed")
                            .to_owned(),
                        tokio::runtime::Handle::current().id(),
                    ))
                    .unwrap();
                timer
            });
            Box::pin(SetupFuture {
                fixture: fixture.clone(),
                mode: config.mode,
                first: true,
                timer,
            }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
        };
        let hooks = RuntimeHooks::new(
            move |_| {
                if let Some(view) = request.view.lock().unwrap().as_ref() {
                    let slot = view.inner.resources.lock().unwrap().reinitialize.clone();
                    if slot.try_lock().is_err() {
                        request.request_off_lock.store(false, Ordering::Release);
                    }
                }
                request.requests.fetch_add(1, Ordering::SeqCst);
                *request.wake.0.lock().unwrap() = true;
                request.wake.1.notify_all();
                request.events.send(Event::Request).unwrap();
                if request.hook_failure.load(Ordering::Acquire) == 1 {
                    std::panic::panic_any(HookPayload(request.clone()));
                }
            },
            move || {
                let worker = finish.worker.lock().unwrap().take();
                if let Some(worker) = worker {
                    let denied = worker.join().unwrap();
                    finish.joins.fetch_add(1, Ordering::SeqCst);
                    if !denied {
                        return TerminalCompletion::Uncertain;
                    }
                }
                finish.finishes.fetch_add(1, Ordering::SeqCst);
                finish.events.send(Event::Finish).unwrap();
                match finish.hook_failure.load(Ordering::Acquire) {
                    2 => std::panic::panic_any(HookPayload(finish.clone())),
                    3 => TerminalCompletion::Uncertain,
                    _ => TerminalCompletion::PhysicallyStopped,
                }
            },
        );
        Ok((Box::new(setup), hooks))
    }
    module_export_static! {
        terminal_setup_factory = factory, config = Config, worker_threads = 1,
        provides = ["ai.tinyhumans.tinybus.Terminal"], methods = [], signals = [],
        requires = [], optional = [], lazy = false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_hook_panics_and_uncertain_finish_preserve_once_join_and_anchor() {
    let _serial = controlled::TEST_LOCK.lock().await;
    for mode in [1, 2, 3] {
        let (fixture, events) = controlled::fixture();
        fixture.hook_failure.store(mode, Ordering::Release);
        let broker = tinybus::broker::Broker::new();
        broker.clone().spawn(tinybus::transport::memory::MemoryBus::new());
        let host = Arc::new(tinybus::module::ModuleHost::new(broker));
        host.attach_linked_with_config(
            controlled::linked_module().unwrap(), serde_json::json!({"mode":1}),
        ).unwrap();
        let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
        let view = fixture.view.lock().unwrap().take().unwrap();
        let context = view.inner.context();
        // Real setup has launched the independent controller worker. Actual
        // request closes/wakes it, and original finish really joins it even
        // when request panic has already made uncertainty sticky.
        view.inner.signal(TerminalReason::Stop);
        host.shutdown(Duration::from_secs(5)).await;
        let completed = view.inner.wait(Duration::from_secs(5));
        let observations: Vec<_> = events.try_iter().collect();
        let native = view.inner.host.0;
        let flags = unsafe { native.native_terminal_state.unwrap()(native.host_ctx) };
        // Controlled work and actual original finish/join have settled before
        // assertions. Uncertain primary payload/resource ownership stays anchored.
        assert!(matches!(entered, controlled::Event::Poll(1)));
        assert_eq!(completed, TerminalCompletion::Uncertain, "hook mode {mode}");
        assert!(view.inner.request_done_for_test());
        assert_eq!(fixture.requests.load(Ordering::Acquire), 1);
        assert_eq!(fixture.finishes.load(Ordering::Acquire), 1);
        assert_eq!(fixture.joins.load(Ordering::Acquire), 1);
        assert_eq!(fixture.payload_drops.load(Ordering::Acquire), 0);
        assert!(!view.is_live());
        assert!(terminal::TerminalState::lookup(context).is_some());
        if mode != 3 {
            assert_ne!(flags & tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT, 0);
        }
        let future_drop = observations.iter().position(|event| matches!(event,
            controlled::Event::FutureDrop(name) if name == "tinybus-terminal")).unwrap();
        let finish = observations.iter().position(|event| matches!(event, controlled::Event::Finish)).unwrap();
        assert!(future_drop < finish);
        // Neither a later cause upgrade nor shutdown retry spends hooks again.
        view.request_fault();
        host.shutdown(Duration::from_millis(20)).await;
        assert_eq!(fixture.requests.load(Ordering::Acquire), 1);
        assert_eq!(fixture.finishes.load(Ordering::Acquire), 1);
        assert_eq!(fixture.payload_drops.load(Ordering::Acquire), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_rejected_physical_spawn_retains_received_resources_without_retry() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker.clone().spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(), serde_json::json!({"mode":1}),
    ).unwrap();
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    let context = view.inner.context();
    view.inner.reject_worker_spawn_for_test();
    view.request_fault();
    host.shutdown(Duration::from_secs(3)).await;
    // Submission failure cannot execute original finish. The fixture alone
    // releases/joins its independent worker; this is not SDK finish evidence.
    fixture.settle_rejected_worker();
    let completed = view.inner.wait(Duration::from_secs(1));
    let observations: Vec<_> = events.try_iter().collect();
    let native = view.inner.host.0;
    let flags = unsafe { native.native_terminal_state.unwrap()(native.host_ctx) };
    assert!(matches!(entered, controlled::Event::Poll(1)));
    assert_eq!(completed, TerminalCompletion::Uncertain);
    assert!(view.inner.request_done_for_test());
    assert_ne!(flags & tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT, 0);
    assert_eq!(fixture.requests.load(Ordering::Acquire), 1);
    assert_eq!(fixture.finishes.load(Ordering::Acquire), 0);
    assert_eq!(fixture.joins.load(Ordering::Acquire), 1);
    assert!(!observations.iter().any(|event| matches!(event,
        controlled::Event::FactoryDrop(_) | controlled::Event::FutureDrop(_) | controlled::Event::Finish)));
    assert!(!view.is_live());
    drop(host);
    view.request_fault();
    assert_eq!(view.inner.wait(Duration::from_millis(20)), TerminalCompletion::Uncertain);
    assert_eq!(fixture.requests.load(Ordering::Acquire), 1);
    assert_eq!(fixture.finishes.load(Ordering::Acquire), 0);
    let anchored = terminal::TerminalState::lookup(context).unwrap();
    let resources = anchored.resources.lock().unwrap();
    assert!(resources.retained_factory.is_some());
    assert!(resources.runtime.is_some());
    assert!(resources.initial.is_some());
    assert!(resources.initial_future.lock().unwrap().is_some());
    // One deliberately retained, bounded original State/job/Runtime remains.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_initial_cancellation_drops_future_and_joins_worker_on_physical_thread() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":1}),
    )
    .unwrap();
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    let shutdown = host.shutdown(Duration::from_secs(5));
    shutdown.await;
    let remainder: Vec<_> = events.try_iter().collect();
    let request_count = fixture.requests.load(Ordering::SeqCst);
    let joins = fixture.joins.load(Ordering::SeqCst);
    // All actual workers/futures settled before asserting behavior.
    assert!(matches!(entered, controlled::Event::Poll(1)));
    assert_eq!(request_count, 1);
    assert_eq!(joins, 1);
    assert!(!view.is_live());
    let future_drop = remainder.iter().position(|event| matches!(event, controlled::Event::FutureDrop(name) if name == "tinybus-terminal")).unwrap();
    let finish = remainder
        .iter()
        .position(|event| matches!(event, controlled::Event::Finish))
        .unwrap();
    assert!(future_drop < finish);
    assert!(remainder.iter().any(
        |event| matches!(event, controlled::Event::FactoryDrop(name) if name == "tinybus-terminal")
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_setup_construction_and_poll_panics_still_finish_real_worker_once() {
    let _serial = controlled::TEST_LOCK.lock().await;
    for mode in [2, 4] {
        let (fixture, events) = controlled::fixture();
        let broker = tinybus::broker::Broker::new();
        broker
            .clone()
            .spawn(tinybus::transport::memory::MemoryBus::new());
        let host = Arc::new(tinybus::module::ModuleHost::new(broker));
        host.attach_linked_with_config(
            controlled::linked_module().unwrap(),
            serde_json::json!({"mode":mode}),
        )
        .unwrap();
        // Consume through actual join notification before assertions or host teardown.
        let mut seen = Vec::new();
        loop {
            let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
            let finished = matches!(event, controlled::Event::Finish);
            seen.push(event);
            if finished {
                break;
            }
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
struct NativeControls {
    ctx: *mut c_void,
    event: unsafe extern "C" fn(*mut c_void, u32, u32),
}
unsafe extern "C" fn native_event(ctx: *mut c_void, event: u32, value: u32) {
    let sender = unsafe { &*ctx.cast::<std::sync::mpsc::Sender<(u32, u32)>>() };
    let _ = sender.send((event, value));
}
// Only fixture symbols are looked up; TinyBus itself verifies/adopts the pinned
// private artifact through the actual ModuleHost loader first. It remains mapped.
#[cfg(unix)]
unsafe fn terminal_fixture_symbols(
    path: &std::path::Path,
) -> (
    extern "C" fn() -> usize,
    extern "C" fn(usize) -> i32,
    extern "C" fn() -> usize,
    unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void, u32, u32)),
    extern "C" fn() -> u32,
) {
    use std::os::unix::ffi::OsStrExt;
    #[cfg_attr(target_os = "linux", link(name = "dl"))]
    unsafe extern "C" {
        fn dlopen(path: *const std::ffi::c_char, flags: i32) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const std::ffi::c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> i32;
    }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let handle = unsafe { dlopen(path.as_ptr(), 2) };
    assert!(!handle.is_null());
    let context = unsafe { dlsym(handle, c"terminal_test_context".as_ptr()) };
    let probe = unsafe { dlsym(handle, c"terminal_test_old_context".as_ptr()) };
    let drops = unsafe { dlsym(handle, c"terminal_test_drops".as_ptr()) };
    let early = unsafe { dlsym(handle, c"terminal_test_early".as_ptr()) };
    let flags = unsafe { dlsym(handle, c"terminal_test_native_flags".as_ptr()) };
    assert!(
        !context.is_null()
            && !probe.is_null()
            && !drops.is_null()
            && !early.is_null()
            && !flags.is_null()
    );
    unsafe {
        dlclose(handle);
    }
    unsafe {
        (
            std::mem::transmute::<*mut c_void, extern "C" fn() -> usize>(context),
            std::mem::transmute::<*mut c_void, extern "C" fn(usize) -> i32>(probe),
            std::mem::transmute::<*mut c_void, extern "C" fn() -> usize>(drops),
            std::mem::transmute::<
                *mut c_void,
                unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void, u32, u32)),
            >(early),
            std::mem::transmute::<*mut c_void, extern "C" fn() -> u32>(flags),
        )
    }
}
#[cfg(windows)]
unsafe fn terminal_fixture_symbols(
    path: &std::path::Path,
) -> (
    extern "C" fn() -> usize,
    extern "C" fn(usize) -> i32,
    extern "C" fn() -> usize,
    unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void, u32, u32)),
    extern "C" fn() -> u32,
) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut c_void;
        fn GetProcAddress(handle: *mut c_void, name: *const u8) -> *mut c_void;
        fn FreeLibrary(handle: *mut c_void) -> i32;
    }
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe { LoadLibraryW(path.as_ptr()) };
    assert!(!handle.is_null());
    let context = unsafe { GetProcAddress(handle, c"terminal_test_context".as_ptr().cast()) };
    let probe = unsafe { GetProcAddress(handle, c"terminal_test_old_context".as_ptr().cast()) };
    let drops = unsafe { GetProcAddress(handle, c"terminal_test_drops".as_ptr().cast()) };
    let early = unsafe { GetProcAddress(handle, c"terminal_test_early".as_ptr().cast()) };
    let flags = unsafe { GetProcAddress(handle, c"terminal_test_native_flags".as_ptr().cast()) };
    assert!(
        !context.is_null()
            && !probe.is_null()
            && !drops.is_null()
            && !early.is_null()
            && !flags.is_null()
    );
    unsafe {
        FreeLibrary(handle);
    }
    unsafe {
        (
            std::mem::transmute::<*mut c_void, extern "C" fn() -> usize>(context),
            std::mem::transmute::<*mut c_void, extern "C" fn(usize) -> i32>(probe),
            std::mem::transmute::<*mut c_void, extern "C" fn() -> usize>(drops),
            std::mem::transmute::<
                *mut c_void,
                unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void, u32, u32)),
            >(early),
            std::mem::transmute::<*mut c_void, extern "C" fn() -> u32>(flags),
        )
    }
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
    let mut dropped_total = 0;
    for mode in [1u8, 0, 2, 4] {
        let (events, receiver) = std::sync::mpsc::channel::<(u32, u32)>();
        // If a controlled failure cannot prove teardown, conservatively retain
        // this one fixture callback owner; never free live foreign context.
        let events = Box::into_raw(Box::new(events));
        let controls = NativeControls {
            ctx: events.cast(),
            event: native_event,
        };
        let encoded = unsafe {
            std::slice::from_raw_parts(
                (&controls as *const NativeControls).cast::<u8>(),
                size_of::<NativeControls>(),
            )
        }
        .to_vec();
        let broker = tinybus::broker::Broker::new();
        broker
            .clone()
            .spawn(tinybus::transport::memory::MemoryBus::new());
        let host = Arc::new(tinybus::module::ModuleHost::new(broker));
        host.load_file_with_config(
            &path,
            serde_json::json!({"mode":mode,"test_controls":encoded}),
        )
        .unwrap();
        let (context, old_probe, drops, _, _) = unsafe { terminal_fixture_symbols(&path) };
        let dropped_before = dropped_total;
        let actual_context = context();
        let first = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        let stale_result = old_context.map(|token| old_probe(token));
        host.shutdown(Duration::from_secs(5)).await;
        let mut seen = vec![first];
        while !seen.iter().any(|(event, _)| *event == 6) {
            seen.push(receiver.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while drops() == dropped_before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        dropped_total = drops();
        unsafe {
            drop(Box::from_raw(events));
        }
        // All actual controller workers and SDK captures are destroyed before assertions.
        if let Some(code) = stale_result {
            assert_eq!(
                code, TB_CLOSED,
                "retired successful context must not act on replacement mode {mode}"
            );
        }
        assert_eq!(seen.iter().filter(|(event, _)| *event == 3).count(), 1);
        assert_eq!(
            seen.iter()
                .filter(|(event, value)| *event == 5 && *value == 1)
                .count(),
            1
        );
        assert_eq!(
            seen.iter()
                .filter(|(event, value)| *event == 7 && *value == 1)
                .count(),
            1
        );
        assert!(seen.iter().any(|(event, value)| *event == 6 && *value == 1));
        if mode != 4 {
            let drop = seen
                .iter()
                .position(|(event, value)| *event == 4 && *value == 1)
                .unwrap();
            let finish = seen.iter().position(|(event, _)| *event == 5).unwrap();
            assert!(drop < finish);
        }
        if mode == 1 {
            old_context = Some(actual_context);
        }
    }

    // The image was admitted by the real loader above. Symbol lookup releases
    // its temporary OS handle: only the canonical loader pins the code now.
    let (_, _, drops, early, native_flags) = unsafe { terminal_fixture_symbols(&path) };
    let (sender, receiver) = std::sync::mpsc::channel();
    let controls = Box::into_raw(Box::new(HeldNativeControls {
        sender,
        release: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
    }));
    unsafe {
        early(controls.cast(), held_native_event);
    }
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = tinybus::module::ModuleHost::new(broker);
    let result =
        host.load_file_with_config(&path, serde_json::json!({"mode":0,"test_controls":[]}));
    let mut seen = Vec::new();
    let entered = loop {
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => {
                seen.push(event);
                if event.0 == 8 {
                    break true;
                }
            }
            Err(_) => break false,
        }
    };
    let flags_before_drop = native_flags();
    drop(host);
    let flags_after_host_drop = native_flags();
    // Release the actual physical callback before any assertion, even if
    // rejected admission or host bookkeeping disagrees with the contract.
    unsafe {
        *(*controls)
            .release
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        (*controls).release.1.notify_all();
    }
    while !seen.iter().any(|event| event.0 == 6) {
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => seen.push(event),
            Err(_) => break,
        }
    }
    let settled = tokio::time::timeout(Duration::from_secs(5), async {
        while drops() == dropped_total {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let final_flags = native_flags();
    // On an unresolved controlled failure retain this one callback context.
    // Otherwise the Acquire fence proves its final foreign use returned.
    if settled {
        unsafe {
            drop(Box::from_raw(controls));
        }
    }
    assert!(
        result.is_err(),
        "source Fault before vtable publication must reject init"
    );
    assert!(
        entered && settled,
        "actual SDK finish and resource Drop must settle independently"
    );
    for flags in [flags_before_drop, flags_after_host_drop, final_flags] {
        assert_ne!(flags & tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT, 0);
        assert_eq!(
            flags & tinybus::module::abi::TB_NATIVE_TERMINAL_PHYSICALLY_STOPPED,
            0,
            "no accepted vtable means no host physical-worker evidence"
        );
    }
    assert_eq!(seen.iter().filter(|event| event.0 == 3).count(), 1);
    assert!(seen.contains(&(5, 1)));
    assert!(seen.contains(&(6, 1)));
}

struct HeldNativeControls {
    sender: std::sync::mpsc::Sender<(u32, u32)>,
    release: (std::sync::Mutex<bool>, std::sync::Condvar),
}
unsafe extern "C" fn held_native_event(ctx: *mut c_void, event: u32, value: u32) {
    let controls = unsafe { &*ctx.cast::<HeldNativeControls>() };
    let _ = controls.sender.send((event, value));
    if event == 8 {
        let mut release = controls
            .release
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*release {
            release = controls
                .release
                .1
                .wait(release)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_linked_reinit_terminal_cancellation_settles_before_worker_join() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":0}),
    )
    .unwrap();
    let initial = events.recv_timeout(Duration::from_secs(3)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !host
            .list()
            .iter()
            .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reinit_host = host.clone();
    let reinit = tokio::spawn(async move {
        reinit_host
            .reinitialize("tinybus-module", serde_json::json!({"mode":1}))
            .await
    });
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    view.request_fault();
    let reinit_result = reinit.await.unwrap();
    host.shutdown(Duration::from_secs(5)).await;
    let mut remainder = Vec::new();
    while !remainder
        .iter()
        .any(|event| matches!(event, controlled::Event::FactoryDrop(_)))
    {
        remainder.push(events.recv_timeout(Duration::from_secs(5)).unwrap());
    }
    assert!(matches!(initial, controlled::Event::Poll(0)));
    assert!(matches!(entered, controlled::Event::Poll(1)));
    assert!(reinit_result.is_err());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
    assert!(!view.is_live());
    let drops: Vec<_> = remainder
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event, controlled::Event::FutureDrop(name) if name == "tinybus-terminal")
                .then_some(index)
        })
        .collect();
    let finish = remainder
        .iter()
        .position(|event| matches!(event, controlled::Event::Finish))
        .unwrap();
    assert_eq!(drops.len(), 2);
    assert!(drops.into_iter().all(|index| index < finish));
}

// A real served object, rather than a synthetic transport owner: Inner destroys
// its transport before its ObjectTree, so the old count can reach zero here.
struct TeardownObject {
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<std::sync::Barrier>,
    settled: std::sync::mpsc::Sender<()>,
    panic: bool,
}
#[async_trait]
impl tinybus::Interface for TeardownObject {
    fn name(&self) -> tinybus::InterfaceName {
        "ai.tinyhumans.tinybus.Teardown".parse().unwrap()
    }
    fn members(&self) -> Vec<tinybus::MemberName> {
        Vec::new()
    }
    async fn call(
        &self,
        _: &tinybus::MemberName,
        _: serde_json::Value,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}
impl Drop for TeardownObject {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        self.release.wait();
        self.settled.send(()).unwrap();
        if self.panic {
            panic!("controlled ObjectTree teardown");
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_connection_object_drop_after_runtime_timeout_denies_physical_completion() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":0}),
    )
    .unwrap();
    events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (settled_tx, settled_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let connection = view
        .inner
        .resources
        .lock()
        .unwrap()
        .connection
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    connection
        .serve_at(
            "/teardown".parse().unwrap(),
            TeardownObject {
                entered: entered_tx,
                release: release.clone(),
                settled: settled_tx,
                panic: false,
            },
        )
        .await
        .unwrap();
    drop(connection);
    let stopping_host = host.clone();
    let stopping = tokio::spawn(async move {
        stopping_host.shutdown(Duration::from_secs(8)).await;
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    stopping.await.unwrap(); // The SDK's real shutdown_timeout has elapsed.
    let completion = view.inner.wait(Duration::ZERO);
    let flags =
        unsafe { (view.inner.host.0.native_terminal_state.unwrap())(view.inner.host.0.host_ctx) };
    let still_anchored = terminal::TerminalState::lookup(view.inner.context()).is_some();
    // Release and settle actual foreign work before reporting a failed proof.
    release.wait();
    settled_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    // Only the retained registry anchor and this test's view may remain.
    // This waits for actual SDK Connection/task owners to finish dropping,
    // rather than treating the object's last event as whole-Inner settlement.
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&view.inner) > 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completion, TerminalCompletion::Uncertain);
    assert_eq!(
        flags & tinybus::module::abi::TB_NATIVE_TERMINAL_PHYSICALLY_STOPPED,
        0
    );
    assert!(
        still_anchored,
        "blocked whole-Connection teardown cannot refund admission"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_connection_object_drop_panic_fences_fault_after_owner_suppression() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":0}),
    )
    .unwrap();
    events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().take().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (settled_tx, settled_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let connection = view
        .inner
        .resources
        .lock()
        .unwrap()
        .connection
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    connection
        .serve_at(
            "/teardown".parse().unwrap(),
            TeardownObject {
                entered: entered_tx,
                release: release.clone(),
                settled: settled_tx,
                panic: true,
            },
        )
        .await
        .unwrap();
    drop(connection);
    let stopping_host = host.clone();
    let stopping = tokio::spawn(async move {
        stopping_host.shutdown(Duration::from_secs(8)).await;
    });
    entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    release.wait();
    settled_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    stopping.await.unwrap(); // Runtime teardown settles the real panicking destructor.
    let completion = view.inner.wait(Duration::ZERO);
    let flags =
        unsafe { (view.inner.host.0.native_terminal_state.unwrap())(view.inner.host.0.host_ctx) };
    let still_anchored = terminal::TerminalState::lookup(view.inner.context()).is_some();

    // Only the retained registry anchor and this test's view may remain.
    // This waits for actual SDK Connection/task owners to finish dropping,
    // rather than treating the object's last event as whole-Inner settlement.
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&view.inner) > 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completion, TerminalCompletion::Uncertain);
    assert_eq!(
        flags & tinybus::module::abi::TB_NATIVE_TERMINAL_PHYSICALLY_STOPPED,
        0
    );
    assert!(
        still_anchored,
        "panicking Connection teardown cannot refund admission"
    );
    assert_ne!(flags & tinybus::module::abi::TB_NATIVE_TERMINAL_FAULT, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_repeated_reinit_disposes_prior_future_on_same_physical_worker_off_lock() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":0}),
    )
    .unwrap();
    events.recv_timeout(Duration::from_secs(3)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !host
            .list()
            .iter()
            .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let first = host
        .reinitialize("tinybus-module", serde_json::json!({"mode":0}))
        .await;
    let second = host
        .reinitialize("tinybus-module", serde_json::json!({"mode":0}))
        .await;
    host.shutdown(Duration::from_secs(8)).await;
    let drops: Vec<_> = events
        .try_iter()
        .filter_map(|event| {
            if let controlled::Event::FutureDrop(name) = event {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    assert!(first.is_ok() && second.is_ok());
    assert_eq!(drops.len(), 3);
    assert!(
        drops.iter().all(|name| name == "tinybus-terminal"),
        "all completed futures must be destroyed by the sole physical worker: {drops:?}"
    );
    assert!(fixture.drop_off_lock.load(Ordering::Acquire));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_reinit_source_error_and_panic_request_outside_callback_exclusion() {
    let _serial = controlled::TEST_LOCK.lock().await;
    for mode in [3, 2, 4] {
        let (fixture, events) = controlled::fixture();
        let broker = tinybus::broker::Broker::new();
        broker
            .clone()
            .spawn(tinybus::transport::memory::MemoryBus::new());
        let host = Arc::new(tinybus::module::ModuleHost::new(broker));
        host.attach_linked_with_config(
            controlled::linked_module().unwrap(),
            serde_json::json!({"mode":0}),
        )
        .unwrap();
        events.recv_timeout(Duration::from_secs(3)).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !host
                .list()
                .iter()
                .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let result = host
            .reinitialize("tinybus-module", serde_json::json!({"mode":mode}))
            .await;
        host.shutdown(Duration::from_secs(8)).await;
        loop {
            if matches!(
                events.recv_timeout(Duration::from_secs(5)).unwrap(),
                controlled::Event::Finish
            ) {
                break;
            }
        }
        assert!(result.is_err());
        assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
        assert!(
            fixture.request_off_lock.load(Ordering::Acquire),
            "source fault request held reinitializer mutex in mode {mode}"
        );
    }
}

async fn controlled_prior_reinit_drop(panic: bool) {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":0}),
    )
    .unwrap();
    events.recv_timeout(Duration::from_secs(3)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !host
            .list()
            .iter()
            .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.reinitialize("tinybus-module", serde_json::json!({"mode":0}))
        .await
        .unwrap();
    let _ = events.try_iter().count();
    let release = Arc::new(std::sync::Barrier::new(2));
    *fixture.future_drop_control.lock().unwrap() = Some((release.clone(), panic));
    let caller_host = host.clone();
    let caller = tokio::spawn(async move {
        caller_host
            .reinitialize(
                "tinybus-module",
                serde_json::json!({"mode":0,"record_drop":true}),
            )
            .await
    });
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().as_ref().unwrap().clone();
    view.inner.signal(TerminalReason::Stop);
    let not_finished_while_blocked = fixture.joins.load(Ordering::SeqCst) == 0;
    let no_next_setup = !events
        .try_iter()
        .any(|event| matches!(event, controlled::Event::Poll(_)));
    release.wait();
    let result = caller.await.unwrap();
    loop {
        if matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            controlled::Event::Finish
        ) {
            break;
        }
    }
    host.shutdown(Duration::from_secs(8)).await;
    let completion = view.inner.wait(Duration::from_secs(5));
    assert_eq!(
        fixture.config_drops.load(Ordering::SeqCst),
        0,
        "rejected reinit must not receive a config before prior retirement and LIVE admission"
    );
    assert!(matches!(entered, controlled::Event::FutureDrop(name) if name == "tinybus-terminal"));
    assert!(not_finished_while_blocked && no_next_setup);
    assert!(result.is_err());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
    assert!(fixture.drop_off_lock.load(Ordering::Acquire));
    assert!(fixture.request_off_lock.load(Ordering::Acquire));
    if panic {
        assert_eq!(completion, TerminalCompletion::Uncertain);
        assert!(view.inner.has_fault());
        assert!(terminal::TerminalState::lookup(view.inner.context()).is_some());
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, controlled::Event::FactoryDrop(_))),
            "remaining foreign captures must stay anchored after first cleanup panic"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_prior_reinit_drop_blocks_next_setup_then_stop_settles_same_job() {
    controlled_prior_reinit_drop(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_prior_reinit_drop_panic_retains_resources_but_attempts_finish_once() {
    controlled_prior_reinit_drop(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_reinit_config_is_destroyed_on_physical_worker_including_constructor_panic() {
    let _serial = controlled::TEST_LOCK.lock().await;
    for mode in [0, 4] {
        let (fixture, events) = controlled::fixture();
        let broker = tinybus::broker::Broker::new();
        broker
            .clone()
            .spawn(tinybus::transport::memory::MemoryBus::new());
        let host = Arc::new(tinybus::module::ModuleHost::new(broker));
        host.attach_linked_with_config(
            controlled::linked_module().unwrap(),
            serde_json::json!({"mode":0}),
        )
        .unwrap();
        events.recv_timeout(Duration::from_secs(3)).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !host
                .list()
                .iter()
                .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let result = host
            .reinitialize(
                "tinybus-module",
                serde_json::json!({"mode":mode,"record_drop":true}),
            )
            .await;
        host.shutdown(Duration::from_secs(8)).await;
        let mut names = Vec::new();
        loop {
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                controlled::Event::ConfigDrop(name) => names.push(name),
                controlled::Event::Finish => break,
                _ => {}
            }
        }
        let view = fixture.view.lock().unwrap().as_ref().unwrap().clone();
        let _completion = view.inner.wait(Duration::from_secs(5));
        assert_eq!(result.is_ok(), mode == 0);
        assert_eq!(fixture.config_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            names,
            ["tinybus-terminal"],
            "accepted generic config Drop must run on the one physical worker"
        );
        assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_reinit_parse_retains_received_config_until_physical_cleanup() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":0}),
    )
    .unwrap();
    events.recv_timeout(Duration::from_secs(3)).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !host
            .list()
            .iter()
            .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let release = Arc::new(std::sync::Barrier::new(2));
    *fixture.config_parse_control.lock().unwrap() = Some(release.clone());
    let caller_host = host.clone();
    let caller = tokio::spawn(async move {
        caller_host
            .reinitialize(
                "tinybus-module",
                serde_json::json!({"mode":0,"record_drop":true,"parse_gate":true}),
            )
            .await
    });
    let entered = events.recv_timeout(Duration::from_secs(3)).unwrap();
    let view = fixture.view.lock().unwrap().as_ref().unwrap().clone();
    view.inner.signal(TerminalReason::Stop);
    let drops_while_blocked = fixture.config_drops.load(Ordering::SeqCst);
    let joins_while_blocked = fixture.joins.load(Ordering::SeqCst);
    release.wait();
    let result = caller.await.unwrap();
    loop {
        if matches!(
            events.recv_timeout(Duration::from_secs(5)).unwrap(),
            controlled::Event::Finish
        ) {
            break;
        }
    }
    host.shutdown(Duration::from_secs(8)).await;
    let completion = view.inner.wait(Duration::from_secs(5));
    let remainder: Vec<_> = events.try_iter().collect();
    assert!(matches!(entered, controlled::Event::ConfigParsing));
    assert_eq!(drops_while_blocked, 0);
    assert_eq!(joins_while_blocked, 0);
    assert!(result.is_err());
    assert_eq!(completion, TerminalCompletion::PhysicallyStopped);
    assert_eq!(fixture.config_drops.load(Ordering::SeqCst), 1);
    assert!(remainder.iter().any(
        |event| matches!(event, controlled::Event::ConfigDrop(name) if name == "tinybus-terminal")
    ));
    assert!(
        !remainder
            .iter()
            .any(|event| matches!(event, controlled::Event::Poll(_)))
    );
    assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn linked_reinit_synchronous_timer_uses_retained_runtime_on_same_worker() {
    let _serial = controlled::TEST_LOCK.lock().await;
    let caller_runtime = tokio::runtime::Handle::current().id();
    let (fixture, events) = controlled::fixture();
    let broker = tinybus::broker::Broker::new();
    broker
        .clone()
        .spawn(tinybus::transport::memory::MemoryBus::new());
    let host = Arc::new(tinybus::module::ModuleHost::new(broker));
    host.attach_linked_with_config(
        controlled::linked_module().unwrap(),
        serde_json::json!({"mode":5}),
    )
    .unwrap();
    let mut observed = vec![events.recv_timeout(Duration::from_secs(3)).unwrap()];
    tokio::time::timeout(Duration::from_secs(3), async {
        while !host
            .list()
            .iter()
            .any(|info| matches!(info.state, tinybus::module::ModuleState::Ready))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let first = host
        .reinitialize("tinybus-module", serde_json::json!({"mode":5}))
        .await;
    let second = host
        .reinitialize("tinybus-module", serde_json::json!({"mode":5}))
        .await;
    let caller_restored = tokio::runtime::Handle::current().id() == caller_runtime;
    let view = fixture.view.lock().unwrap().as_ref().unwrap().clone();
    host.shutdown(Duration::from_secs(8)).await;
    let completion = view.inner.wait(Duration::from_secs(5));
    let after_stop = host
        .reinitialize("tinybus-module", serde_json::json!({"mode":5}))
        .await;
    observed.extend(events.try_iter());
    let constructors: Vec<_> = observed
        .iter()
        .filter_map(|event| match event {
            controlled::Event::TimerConstruct(thread, name, runtime) => {
                Some((*thread, name.as_str(), *runtime))
            }
            _ => None,
        })
        .collect();
    // Actual caller, controller worker and SDK cleanup have settled before assertions,
    // including the unfixed constructor-panic path.
    assert!(
        first.is_ok() && second.is_ok(),
        "synchronous timer reinit failed: {first:?}, {second:?}"
    );
    assert_eq!(completion, TerminalCompletion::PhysicallyStopped);
    assert_eq!(fixture.joins.load(Ordering::SeqCst), 1);
    assert_eq!(constructors.len(), 3);
    assert_eq!(constructors[1].1, "tinybus-terminal");
    assert_eq!(constructors[2].1, "tinybus-terminal");
    assert_eq!(constructors[1].0, constructors[2].0);
    assert!(
        constructors
            .iter()
            .all(|entry| entry.2 == constructors[0].2)
    );
    assert_ne!(constructors[0].2, caller_runtime);
    assert!(caller_restored);
    assert!(after_stop.is_err());
    let cleanup_contexts: Vec<_> = observed
        .iter()
        .filter_map(|event| match event {
            controlled::Event::TimerDrop(context) => Some(context),
            _ => None,
        })
        .collect();
    assert_eq!(cleanup_contexts.len(), 3);
    assert!(
        cleanup_contexts.iter().all(|context| context.is_none()),
        "constructor runtime guard must be restored before later physical-worker cleanup"
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event, controlled::Event::Poll(5)))
            .count(),
        3
    );
}
