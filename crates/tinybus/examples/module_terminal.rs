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
struct Controller {
    controls: Mutex<Option<Controls>>,
    view: NativeInstanceLiveness,
    wake: (Mutex<bool>, Condvar),
    worker: Mutex<Option<std::thread::JoinHandle<bool>>>,
}
impl Controller {
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
        self.emit(
            6,
            u32::from(std::thread::current().name() == Some("tinybus-terminal")),
        );
        DROPPED.fetch_add(1, Ordering::Release);
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
            self.controller.emit(2, u32::from(self.mode));
        }
        match self.mode {
            0 => Poll::Ready(Ok(())),
            2 => panic!("controlled native setup poll"),
            5 => Poll::Ready(Err(tinybus::Error::failed("controlled native setup error"))),
            _ => Poll::Pending,
        }
    }
}
impl Drop for SetupFuture {
    fn drop(&mut self) {
        self.controller.emit(
            4,
            u32::from(std::thread::current().name() == Some("tinybus-terminal")),
        );
    }
}
fn factory(
    view: NativeInstanceLiveness,
) -> Result<(
    impl Fn(Connection, Config) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync,
    RuntimeHooks,
)> {
    let early = EARLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let held_finish = early.is_some();
    let controller = Arc::new(Controller {
        controls: Mutex::new(early),
        view,
        wake: (Mutex::new(false), Condvar::new()),
        worker: Mutex::new(None),
    });
    if held_finish {
        controller.view.request_fault();
    }
    let (request, finish) = (controller.clone(), controller.clone());
    let setup = move |_: Connection, config: Config| {
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
        if config.mode == 4 {
            panic!("controlled native setup construction");
        }
        Box::pin(SetupFuture {
            controller: controller.clone(),
            mode: config.mode,
            first: true,
        }) as Pin<Box<dyn Future<Output = Result<()>> + Send>>
    };
    let hooks = RuntimeHooks::new(
        move |_| {
            *request
                .wake
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            request.wake.1.notify_all();
            request.emit(3, 1);
        },
        move || {
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
            if joined {
                TerminalCompletion::PhysicallyStopped
            } else {
                TerminalCompletion::Uncertain
            }
        },
    );
    Ok((setup, hooks))
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
        methods: &[],
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
        if let Some(query) = host.native_terminal_state {
            *NATIVE_QUERY
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(NativeQuery {
                ctx: host.host_ctx,
                query,
            });
        }
    }
    let code = unsafe {
        tinybus_module::start_terminal_reconfigurable_module_with_factory::<Config, _, _>(
            host, out, 1, true, factory,
        )
    };
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
