//! Actual opted-in runtime fixture. Controls are trusted test-only C callbacks.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    ffi::c_void,
    future::Future,
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll},
};
use tinybus::{Connection, Result};
static DROPPED: AtomicUsize = AtomicUsize::new(0);
static EARLY: Mutex<Option<Controls>> = Mutex::new(None);
static NEXT_FACTORY_MODE: AtomicUsize = AtomicUsize::new(0);
static FACTORIES: AtomicUsize = AtomicUsize::new(0);
static FACTORY_DISPATCHES: Mutex<Vec<tracing::Dispatch>> = Mutex::new(Vec::new());
static EXTERNAL_CONTROLLER: Mutex<Option<std::sync::Weak<Controller>>> = Mutex::new(None);
static RETAINED_VIEWS: Mutex<Vec<NativeInstanceLiveness>> = Mutex::new(Vec::new());
#[derive(Clone, Copy)]
struct NativeQuery {
    ctx: *mut c_void,
    query: unsafe extern "C" fn(*mut c_void) -> u32,
}
unsafe impl Send for NativeQuery {}
static NATIVE_QUERY: Mutex<Option<NativeQuery>> = Mutex::new(None);
use tinybus::module::abi::*;
use tinybus_module::{NativeInstanceLiveness, RuntimeHooks, TerminalCompletion};
#[derive(serde::Deserialize)]
struct Config {
    mode: u8,
    test_controls: Vec<u8>,
}
impl Drop for Config {
    fn drop(&mut self) {
        if self.test_controls.len() == size_of::<Controls>() {
            let controls = unsafe {
                self.test_controls
                    .as_ptr()
                    .cast::<Controls>()
                    .read_unaligned()
            };
            controls.emit(
                19,
                u32::from(std::thread::current().name() == Some("tinybus-terminal")),
            );
        }
    }
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Controls {
    ctx: *mut c_void,
    event: unsafe extern "C" fn(*mut c_void, u32, u32),
}
unsafe impl Send for Controls {}
unsafe impl Sync for Controls {}
impl Controls {
    fn emit(self, event: u32, value: u32) {
        unsafe { (self.event)(self.ctx, event, value) }
    }
}
struct PrimaryPayload(Option<Controls>);
impl Drop for PrimaryPayload {
    fn drop(&mut self) {
        if let Some(controls) = self.0 {
            controls.emit(17, 1);
        }
    }
}
struct Controller {
    controls: Mutex<Option<Controls>>,
    view: NativeInstanceLiveness,
    mode: AtomicUsize,
    wake: (Mutex<bool>, Condvar),
    worker: Mutex<Option<std::thread::JoinHandle<bool>>>,
}
impl Controller {
    fn observe(&self, event: u32) {
        #[cfg(feature = "__native-fixture-support")]
        self.emit(
            event,
            u32::from(self.view.__fixture_owner_is_current())
                | (u32::from(self.view.__fixture_owner_is_stopped()) << 1)
                | (u32::from(self.view.__fixture_request_done()) << 2)
                | (u32::from(self.view.__fixture_sdk_owners() != 0) << 3),
        );
        #[cfg(not(feature = "__native-fixture-support"))]
        let _ = event;
    }
    fn emit(&self, event: u32, value: u32) {
        let controls = *self
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(controls) = controls {
            controls.emit(event, value);
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.observe(15);
        self.emit(
            6,
            u32::from(std::thread::current().name() == Some("tinybus-terminal")),
        );
        DROPPED.fetch_add(1, Ordering::Release);
    }
}
struct FactoryCapture(Arc<Controller>);
impl FactoryCapture {
    fn controller(&self) -> &Arc<Controller> {
        &self.0
    }
}
impl Drop for FactoryCapture {
    fn drop(&mut self) {
        self.0.emit(
            20,
            u32::from(std::thread::current().name() == Some("tinybus-terminal")),
        );
    }
}
struct SetupFuture {
    controller: Arc<Controller>,
    mode: u8,
    first: bool,
}
impl Future for SetupFuture {
    type Output = Result<()>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        if self.first {
            self.first = false;
            self.controller.observe(11);
            self.controller.emit(2, u32::from(self.mode));
        }
        if self.mode == 41 || self.mode == 43 || self.mode == 45 {
            self.controller.emit(24, u32::from(self.mode));
        }
        match self.mode {
            43 => Poll::Ready(Err(tinybus::Error::failed(
                "ready native reinit source error",
            ))),
            45 => std::panic::panic_any(PrimaryPayload(*self.controller.controls.lock().unwrap())),
            0 | 48 | 49 => Poll::Ready(Ok(())),
            2 => panic!("controlled native setup poll"),
            5 => Poll::Ready(Err(tinybus::Error::failed("controlled native setup error"))),
            _ => Poll::Pending,
        }
    }
}
impl Drop for SetupFuture {
    fn drop(&mut self) {
        if self.mode == 48 || self.mode == 49 {
            self.controller
                .emit(38, u32::from(tokio::runtime::Handle::try_current().is_ok()));
        }
        if self.mode == 42 || self.mode == 47 || self.mode == 48 || self.mode == 49 {
            self.controller.emit(25, 0);
        }
        if self.mode == 47 || self.mode == 49 {
            std::panic::panic_any(PrimaryPayload(*self.controller.controls.lock().unwrap()));
        }
        self.controller.observe(14);
        self.controller.emit(
            4,
            u32::from(std::thread::current().name() == Some("tinybus-terminal")),
        );
    }
}
type FixtureFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type FixtureSetup = Box<dyn Fn(Connection, Config) -> FixtureFuture + Send + Sync>;
fn factory(view: NativeInstanceLiveness) -> Result<(FixtureSetup, RuntimeHooks)> {
    FACTORIES.fetch_add(1, Ordering::AcqRel);
    let factory_mode = NEXT_FACTORY_MODE.swap(0, Ordering::AcqRel);
    if factory_mode == 46 {
        FACTORY_DISPATCHES
            .lock()
            .unwrap()
            .push(tracing::dispatcher::get_default(Clone::clone));
    }
    if factory_mode == 3 || (20..=29).contains(&factory_mode) || factory_mode == 46 {
        RETAINED_VIEWS.lock().unwrap().push(view.clone());
    }
    let early = EARLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(controls) = early {
        controls.emit(9, factory_mode as u32);
    }
    if factory_mode == 1 {
        return Err(tinybus::Error::failed("controlled factory error"));
    }
    if factory_mode == 2 {
        std::panic::panic_any(PrimaryPayload(early));
    }
    let held_finish = early.is_some() && factory_mode == 0;
    let controller = Arc::new(Controller {
        controls: Mutex::new(early),
        view,
        mode: AtomicUsize::new(factory_mode),
        wake: (Mutex::new(false), Condvar::new()),
        worker: Mutex::new(None),
    });
    controller.observe(10);
    controller.observe(27);
    #[cfg(feature = "__native-fixture-support")]
    match factory_mode {
        20 => controller.view.__fixture_reject_worker_spawn(),
        21..=29 => controller
            .view
            .__fixture_reject_startup((factory_mode - 20) as u8),
        _ => {}
    }
    if factory_mode == 20 {
        *EXTERNAL_CONTROLLER.lock().unwrap() = Some(Arc::downgrade(&controller));
    }
    if held_finish {
        controller.view.request_fault();
    }
    let (request, finish) = (controller.clone(), controller.clone());
    let capture = FactoryCapture(controller);
    let setup = move |connection: Connection, config: Config| {
        let controller = capture.controller();
        controller.observe(16);
        controller.emit(26, u32::from(tokio::runtime::Handle::try_current().is_ok()));
        controller.observe(28);
        if config.test_controls.len() != size_of::<Controls>() {
            return Box::pin(async { Err(tinybus::Error::failed("bad test controls")) })
                as Pin<Box<dyn Future<Output = Result<()>> + Send>>;
        }
        // SAFETY: this explicit fixture accepts trusted in-process test pointers.
        let controls = unsafe {
            config
                .test_controls
                .as_ptr()
                .cast::<Controls>()
                .read_unaligned()
        };
        controller
            .mode
            .store(usize::from(config.mode), Ordering::Release);
        *controller
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(controls);
        let mut worker = controller
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if worker.is_none() {
            let work = controller.clone();
            *worker = Some(
                std::thread::Builder::new()
                    .name("native-terminal-controller".into())
                    .spawn(move || {
                        let mut cancelled = work
                            .wake
                            .0
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while !*cancelled {
                            cancelled = work
                                .wake
                                .1
                                .wait(cancelled)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        drop(cancelled);
                        let denied = !work.view.is_live();
                        work.emit(7, u32::from(denied));
                        denied
                    })
                    .expect("controlled worker spawn"),
            );
        }
        drop(worker);
        if config.mode == 40 || config.mode == 44 {
            controller.emit(23, u32::from(config.mode));
        }
        if config.mode == 44 {
            std::panic::panic_any(PrimaryPayload(*controller.controls.lock().unwrap()));
        }
        if config.mode == 4 {
            panic!("controlled native setup construction");
        }
        if (50..=52).contains(&config.mode) {
            let object = TerminalObject {
                controller: controller.clone(),
                mode: config.mode,
            };
            let work = controller.clone();
            return Box::pin(async move {
                connection.serve_at("/terminal".parse()?, object).await?;
                connection
                    .request_name("ai.tinyhumans.TerminalFixture")
                    .await?;
                work.emit(31, 1);
                Ok(())
            }) as FixtureFuture;
        }
        Box::pin(SetupFuture {
            controller: controller.clone(),
            mode: config.mode,
            first: true,
        }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
    };
    let hooks = RuntimeHooks::new(
        move |_| {
            request.observe(12);
            #[cfg(feature = "__native-fixture-support")]
            request.emit(22, request.view.__fixture_resources());
            *request
                .wake
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            request.wake.1.notify_all();
            request.emit(3, 1);
            if request.mode.load(Ordering::Acquire) == 10 {
                std::panic::panic_any(PrimaryPayload(*request.controls.lock().unwrap()));
            }
        },
        move || {
            finish.observe(13);
            if held_finish {
                finish.emit(8, 0);
            }
            let worker = finish
                .worker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let outcome = worker.map(|worker| worker.join());
            let joined = outcome.is_none() || matches!(outcome, Some(Ok(true)));
            finish.emit(5, u32::from(joined));
            match finish.mode.load(Ordering::Acquire) {
                11 => std::panic::panic_any(PrimaryPayload(*finish.controls.lock().unwrap())),
                12 => return TerminalCompletion::Uncertain,
                _ => {}
            }
            if joined {
                TerminalCompletion::PhysicallyStopped
            } else {
                TerminalCompletion::Uncertain
            }
        },
    );
    Ok((Box::new(setup), hooks))
}
#[unsafe(no_mangle)]
pub static TINYBUS_MODULE_ABI_V1: TbAbiDescriptor =
    TbAbiDescriptor::current("module-terminal", env!("CARGO_PKG_VERSION"));
#[unsafe(no_mangle)]
pub extern "C" fn tinybus_module_manifest_v1() -> TbSlice {
    tinybus_module::manifest_slice(tinybus_module::ManifestDeclaration {
        name: "module-terminal",
        version: env!("CARGO_PKG_VERSION"),
        provides: &["ai.tinyhumans.TerminalFixture"],
        methods: &["Panic"],
        signals: &[],
        requires: &[],
        optional: &[],
        lazy: false,
        worker_threads: 1,
    })
}
#[derive(Clone, Copy)]
struct Table(TbModuleVtable);
unsafe impl Send for Table {}
static LATEST: Mutex<Option<Table>> = Mutex::new(None);
/// # Safety
/// Standard trusted module init contract plus live test controls in config.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tinybus_module_init_v1(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    if let Some(host) = unsafe { TbHostVtable::read_compatible(host) } {
        // The fixture needs its trusted callback before factory evaluation.
        // Product authority still comes exclusively from the untouched host query.
        if !host.config.ptr.is_null() {
            let bytes = unsafe { std::slice::from_raw_parts(host.config.ptr, host.config.len) };
            if let Ok(config) = serde_json::from_slice::<serde_json::Value>(bytes) {
                if let Some(encoded) = config
                    .get("test_controls")
                    .and_then(|value| serde_json::from_value::<Vec<u8>>(value.clone()).ok())
                    .filter(|bytes| bytes.len() == size_of::<Controls>())
                {
                    let controls = unsafe { encoded.as_ptr().cast::<Controls>().read_unaligned() };
                    let mut early = EARLY
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if early.is_none() {
                        *early = Some(controls);
                        NEXT_FACTORY_MODE.store(99, Ordering::Release);
                    }
                }
            }
        }
        if let Some(query) = host.native_terminal_state {
            *NATIVE_QUERY
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(NativeQuery {
                ctx: host.host_ctx,
                query,
            });
        }
    }
    if NEXT_FACTORY_MODE.load(Ordering::Acquire) == 30 {
        let controls = *EARLY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(controls) = controls {
            controls.emit(18, 0);
        }
    }
    let factories_before = FACTORIES.load(Ordering::Acquire);
    let code = unsafe {
        tinybus_module::start_terminal_reconfigurable_module_with_factory::<Config, _, _>(
            host, out, 1, true, factory,
        )
    };
    if FACTORIES.load(Ordering::Acquire) == factories_before {
        EARLY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        NEXT_FACTORY_MODE.store(0, Ordering::Release);
    }
    if code == TB_OK {
        *LATEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Table(unsafe { *out }));
    }
    code
}
/// Test-only stale-context probe, using the same actual SDK callback.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_context() -> usize {
    LATEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or(0, |table| table.0.module_ctx.addr())
}
/// Test-only copied callback probe; the token is never dereferenced.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_old_context(token: usize) -> i32 {
    let table = *LATEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    table.map_or(TB_CLOSED, |table| unsafe {
        (table.0.shutdown)(std::ptr::with_exposed_provenance_mut(token), 1000)
    })
}

/// Test-only acquire fence after the final controller event callback returned.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_drops() -> usize {
    DROPPED.load(Ordering::Acquire)
}

/// # Safety
/// The test context and callback must remain live until actual controller Drop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn terminal_test_early(
    ctx: *mut c_void,
    event: unsafe extern "C" fn(*mut c_void, u32, u32),
) {
    *EARLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Controls { ctx, event });
}
/// Sample only the actual copied host query; no local or fabricated success bit.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_native_flags() -> u32 {
    let query = *NATIVE_QUERY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    query.map_or(0, |query| unsafe { (query.query)(query.ctx) })
}

/// # Safety
/// Controls must remain live through all emitted callbacks. Modes only configure
/// this example fixture; they do not grant native permission or construct views.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn terminal_test_factory_mode(
    mode: usize,
    ctx: *mut c_void,
    event: unsafe extern "C" fn(*mut c_void, u32, u32),
) {
    *EARLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Controls { ctx, event });
    NEXT_FACTORY_MODE.store(mode, Ordering::Release);
}
/// Counts actual factory invocations, including those which return no hooks.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_factories() -> usize {
    FACTORIES.load(Ordering::Acquire)
}
/// Samples escaped actual factory-provided views against their copied host query.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_retained_live() -> usize {
    let views = RETAINED_VIEWS.lock().unwrap().clone();
    views.iter().filter(|view| view.is_live()).count()
}
/// Release escaped actual views outside their fixture storage lock.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_release_views() -> usize {
    let views = std::mem::take(&mut *RETAINED_VIEWS.lock().unwrap());
    let count = views.len();
    drop(views);
    count
}
/// Release one genuinely retained view, allowing its actual State last-Drop.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_release_one_view() -> bool {
    let view = {
        let mut views = RETAINED_VIEWS.lock().unwrap();
        if views.is_empty() {
            None
        } else {
            Some(views.remove(0))
        }
    };
    let released = view.is_some();
    drop(view);
    released
}

/// Observes resources of the last genuinely escaped factory view.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_resources() -> u32 {
    #[cfg(feature = "__native-fixture-support")]
    {
        let view = RETAINED_VIEWS.lock().unwrap().last().cloned();
        view.map_or(0, |view| view.__fixture_resources())
    }
    #[cfg(not(feature = "__native-fixture-support"))]
    {
        0
    }
}
/// Observes actual received transport disposal, independently of finish.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_transport_drop() -> bool {
    #[cfg(feature = "__native-fixture-support")]
    {
        let view = RETAINED_VIEWS.lock().unwrap().last().cloned();
        view.is_some_and(|view| view.__fixture_transport_dropped_on_physical_worker())
    }
    #[cfg(not(feature = "__native-fixture-support"))]
    {
        false
    }
}
/// Explicit fixture cleanup after SDK spawn rejection; never claims SDK finish.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_external_join() -> bool {
    let controller = EXTERNAL_CONTROLLER
        .lock()
        .unwrap()
        .take()
        .and_then(|weak| weak.upgrade());
    let Some(controller) = controller else {
        return false;
    };
    let worker = controller.worker.lock().unwrap().take();
    let joined = worker.is_some_and(|worker| worker.join().unwrap_or(false));
    controller.emit(21, u32::from(joined));
    joined
}

/// Invoke the exact exported SDK reinitializer with a previously observed token.
/// # Safety
/// Input must remain readable throughout this synchronous callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn terminal_test_reinit(token: usize, ptr: *const u8, len: usize) -> i32 {
    let table = *LATEST.lock().unwrap();
    table
        .and_then(|table| table.0.reinitialize)
        .map_or(TB_CLOSED, |reinit| unsafe {
            reinit(std::ptr::with_exposed_provenance_mut(token), ptr, len)
        })
}
/// Current SDK owner observation on an external caller, with no scope installed.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_unowned() -> bool {
    #[cfg(feature = "__native-fixture-support")]
    {
        let views = RETAINED_VIEWS.lock().unwrap().clone();
        views.iter().all(|view| !view.__fixture_owner_is_current())
    }
    #[cfg(not(feature = "__native-fixture-support"))]
    {
        true
    }
}

struct MethodPayload(Arc<Controller>);
impl Drop for MethodPayload {
    fn drop(&mut self) {
        let query = *NATIVE_QUERY.lock().unwrap();
        let native = query.map_or(0, |query| unsafe { (query.query)(query.ctx) });
        #[cfg(feature = "__native-fixture-support")]
        let evidence = u32::from(native & TB_NATIVE_TERMINAL_FAULT != 0)
            | (u32::from(self.0.view.__fixture_request_done()) << 1)
            | (u32::from(self.0.view.__fixture_sdk_owners() != 0) << 2);
        #[cfg(not(feature = "__native-fixture-support"))]
        let evidence = native;
        // First payload-disposal action reads the actual native query and
        // actual request state before entering the trusted blocking control.
        self.0.emit(29, evidence);
        self.0.emit(30, 0);
        self.0.emit(33, 1);
    }
}
struct TerminalObject {
    controller: Arc<Controller>,
    mode: u8,
}
#[tinybus::interface(name = "ai.tinyhumans.TerminalFixture")]
impl TerminalObject {
    async fn panic(&self) -> Result<()> {
        std::panic::panic_any(MethodPayload(self.controller.clone()));
    }
}
impl Drop for TerminalObject {
    fn drop(&mut self) {
        if self.mode == 51 || self.mode == 52 {
            self.controller.observe(35);
            self.controller.emit(32, u32::from(self.mode));
            self.controller.emit(33, u32::from(self.mode));
            if self.mode == 52 {
                panic!("controlled native Connection object destructor");
            }
        }
    }
}
/// Counts actual surviving SDK Connection/task owners, not messages/futures.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_sdk_owners() -> usize {
    #[cfg(feature = "__native-fixture-support")]
    {
        let view = RETAINED_VIEWS.lock().unwrap().last().cloned();
        view.map_or(0, |view| view.__fixture_sdk_owners())
    }
    #[cfg(not(feature = "__native-fixture-support"))]
    {
        0
    }
}

/// Probe a dispatcher captured by the genuine factory's actual caller scope.
/// It supplies no execution owner or native admission authority.
#[unsafe(no_mangle)]
pub extern "C" fn terminal_test_dispatch(index: usize) -> bool {
    let dispatch = FACTORY_DISPATCHES.lock().unwrap().get(index).cloned();
    dispatch.is_some_and(|dispatch| {
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!("terminal_factory_dispatch");
            let enabled = span
                .metadata()
                .is_some_and(|metadata| dispatch.enabled(metadata));
            tracing::info!("retained native factory dispatcher");
            enabled
        })
    })
}
