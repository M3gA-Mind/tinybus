//! Module-side runtime for trusted tinybus `cdylib` integrations.
//!
//! Each module owns a Tokio runtime. A statically linked `cdylib` has its own
//! Tokio thread-locals, so attempting to borrow the host runtime is both
//! incorrect and capable of silently blocking a host worker thread.
//!
//! Caller context depends on the trusted host's optional ABI-v1 routing callback.
//! Its assertion is captured per frame at enqueue time; older host prefixes
//! remain compatible but supply unverified context. Sender headers remain intact
//! for protocol routing and streams even when they carry no caller authority.

use std::ffi::c_void;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tinybus::message::Message;
use tinybus::message::codec::MAX_FRAME_LEN;
use tinybus::module::abi::{
    TB_BACKPRESSURE, TB_BAD_ARGUMENT, TB_CLOSED, TB_MODULE_VTABLE_BASE_SIZE, TB_OK, TB_PANICKED,
    TB_TIMEOUT, TbHostVtable, TbModuleVtable, TbModuleVtableV1Prefix,
};
use tinybus::{Connection, Error, Result, Transport};
use tokio::sync::{Mutex, mpsc};

mod execution_context;
use execution_context::ExecutionOwner;

const MODULE_QUEUE_CAPACITY: usize = 256;
#[cfg(not(test))]
const MODULE_REINITIALIZE_DEADLINE: Duration = Duration::from_secs(4);
#[cfg(test)]
const MODULE_REINITIALIZE_DEADLINE: Duration = Duration::from_millis(100);
const MODULE_PANIC_ERROR: &str = "ai.tinyhumans.tinybus.Error.ModulePanicked";
static MANIFEST_BYTES: OnceLock<Vec<u8>> = OnceLock::new();

/// Build and retain the exported manifest bytes for the process lifetime.
#[doc(hidden)]
pub struct ManifestDeclaration<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub provides: &'a [&'a str],
    pub methods: &'a [&'a str],
    pub signals: &'a [&'a str],
    pub requires: &'a [&'a str],
    pub optional: &'a [&'a str],
    pub lazy: bool,
    pub worker_threads: u32,
}

/// Build and retain the exported manifest bytes for the process lifetime.
#[doc(hidden)]
pub fn manifest_slice(declaration: ManifestDeclaration<'_>) -> tinybus::module::abi::TbSlice {
    manifest_slice_in(&MANIFEST_BYTES, declaration)
}

/// Retain a manifest in the caller's slot, allowing multiple linked modules.
#[doc(hidden)]
pub fn manifest_slice_in(
    slot: &'static OnceLock<Vec<u8>>,
    declaration: ManifestDeclaration<'_>,
) -> tinybus::module::abi::TbSlice {
    catch_unwind(AssertUnwindSafe(|| build_manifest_slice(slot, declaration))).unwrap_or(
        tinybus::module::abi::TbSlice {
            ptr: std::ptr::null(),
            len: 0,
        },
    )
}

fn build_manifest_slice(
    slot: &'static OnceLock<Vec<u8>>,
    declaration: ManifestDeclaration<'_>,
) -> tinybus::module::abi::TbSlice {
    use tinybus::module::manifest::{
        Dependency, MANIFEST_SCHEMA, ModuleIdentity, ModuleManifest, PanicPolicy, ProvidedInterface,
    };
    use tinybus::{BusName, InterfaceName, InterfaceVersion, ObjectPath, Version};

    let bytes = slot.get_or_init(|| {
        let package_version =
            Version::parse(declaration.version).expect("package version is semver");
        let provided = |(index, interface): (usize, &&str)| ProvidedInterface {
            version: InterfaceVersion::provided(
                InterfaceName::new(*interface).expect("provided interface is valid"),
                package_version.clone(),
            ),
            methods: if index == 0 {
                declaration
                    .methods
                    .iter()
                    .map(|member| tinybus::MemberName::new(*member).expect("method is valid"))
                    .collect()
            } else {
                Vec::new()
            },
            signals: if index == 0 {
                declaration
                    .signals
                    .iter()
                    .map(|member| tinybus::MemberName::new(*member).expect("signal is valid"))
                    .collect()
            } else {
                Vec::new()
            },
        };
        let dependency = |interface: &&str, optional| Dependency {
            interface: InterfaceVersion::consumed(
                InterfaceName::new(*interface).expect("dependency interface is valid"),
                package_version.clone(),
            ),
            optional,
            reason: String::new(),
        };
        let bus_name = declaration
            .provides
            .first()
            .copied()
            .unwrap_or("ai.tinyhumans.module.Empty");
        let object_path = format!("/{}", bus_name.replace('.', "/"));
        serde_json::to_vec(&ModuleManifest {
            schema: MANIFEST_SCHEMA,
            module: ModuleIdentity {
                name: declaration.name.to_string(),
                version: package_version.clone(),
                description: String::new(),
                homepage: None,
                license: String::new(),
            },
            bus_name: BusName::new(bus_name).expect("provided interface is a bus name"),
            object_path: ObjectPath::new(object_path).expect("derived object path is valid"),
            provides: declaration
                .provides
                .iter()
                .enumerate()
                .map(provided)
                .collect(),
            requires: declaration
                .requires
                .iter()
                .map(|interface| dependency(interface, false))
                .chain(
                    declaration
                        .optional
                        .iter()
                        .map(|interface| dependency(interface, true)),
                )
                .collect(),
            environment: Vec::new(),
            capabilities: Vec::new(),
            lazy_init: declaration.lazy,
            worker_threads: declaration.worker_threads,
            on_panic: PanicPolicy::Detach,
        })
        .expect("module manifest is serializable")
    });
    tinybus::module::abi::TbSlice {
        ptr: bytes.as_ptr(),
        len: bytes.len(),
    }
}

#[derive(Clone, Copy)]
struct HostCalls(TbHostVtable);

// The opaque pointer belongs to the host and is explicitly valid for the
// process lifetime. Calls are required to be thread-safe by the ABI contract.
unsafe impl Send for HostCalls {}
unsafe impl Sync for HostCalls {}

impl HostCalls {
    fn is_broker_routed(&self) -> bool {
        self.0
            .broker_routing
            .is_some_and(|routing| unsafe { routing(self.0.host_ctx) } == 1)
    }

    fn send(&self, bytes: &[u8]) -> i32 {
        unsafe { (self.0.send)(self.0.host_ctx, bytes.as_ptr(), bytes.len()) }
    }

    fn wake(&self) {
        unsafe { (self.0.wake)(self.0.host_ctx) }
    }

    fn fault(&self) {
        unsafe { (self.0.fault)(self.0.host_ctx, std::ptr::null(), 0) }
    }

    fn log(&self, level: u32, message: &[u8]) {
        unsafe { (self.0.log)(self.0.host_ctx, level, message.as_ptr(), message.len()) }
    }

    fn ready(&self) {
        unsafe { (self.0.ready)(self.0.host_ctx) }
    }
}

struct HostSubscriber {
    host: HostCalls,
    next_span: AtomicU64,
    max_level: tracing::level_filters::LevelFilter,
}

impl tracing::Subscriber for HostSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.max_level >= *metadata.level()
    }

    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        Some(self.max_level)
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        use std::fmt::Write as _;

        let metadata = event.metadata();
        let mut visitor = LogVisitor(String::new());
        event.record(&mut visitor);
        let mut line = String::new();
        let _ = write!(line, "{} {}", metadata.target(), visitor.0);
        let level = match *metadata.level() {
            tracing::Level::ERROR => 1,
            tracing::Level::WARN => 2,
            tracing::Level::INFO => 3,
            tracing::Level::DEBUG => 4,
            tracing::Level::TRACE => 5,
        };
        self.host.log(level, line.as_bytes());
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

struct LogVisitor(String);

impl tracing::field::Visit for LogVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;

        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = write!(self.0, "{}={value:?}", field.name());
    }
}

// Private envelope captures trusted-host routing before the frame is queued.
type IncomingFrame = (Vec<u8>, bool);

struct ModuleTransport {
    host: HostCalls,
    inbound: Mutex<mpsc::Receiver<IncomingFrame>>,
    provenance: Arc<AtomicBool>,
    detach_on_panic: bool,
}

#[async_trait]
impl Transport for ModuleTransport {
    async fn send(&self, message: Message) -> Result<()> {
        let panicked = message.header.error_name.as_deref() == Some(MODULE_PANIC_ERROR);
        let bytes = serde_json::to_vec(&message)?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(Error::protocol("module frame exceeds the size cap"));
        }
        match self.host.send(&bytes) {
            TB_OK => {
                if panicked && self.detach_on_panic {
                    self.host.fault();
                }
                Ok(())
            }
            TB_BACKPRESSURE => Err(Error::Backpressure),
            TB_CLOSED => Err(Error::ConnectionClosed),
            _ => Err(Error::transport("module host refused a frame")),
        }
    }

    async fn recv(&self) -> Result<Option<Message>> {
        let bytes = self.inbound.lock().await.recv().await;
        let Some((bytes, brokered)) = bytes else {
            return Ok(None);
        };
        self.host.wake();
        let message = serde_json::from_slice(&bytes)?;
        // The connection is the only reader and samples this slot immediately
        // after recv. Keep header identity intact for stream ownership/routing;
        // only contextual dispatch consumes the captured delivery authority.
        self.provenance.store(brokered, Ordering::Release);
        Ok(Some(message))
    }

    async fn close(&self) -> Result<()> {
        self.inbound.lock().await.close();
        Ok(())
    }

    fn describe(&self) -> String {
        "module".to_string()
    }
}

fn module_connection(transport: Arc<ModuleTransport>) -> Connection {
    if transport.host.0.broker_routing.is_some() {
        let provenance = transport.provenance.clone();
        // SAFETY: this private transport has one reader, publishes the queue's
        // per-frame proof before returning, and no other writer touches its
        // slot. The trusted host asserts routing at deliver, not at receive.
        unsafe { Connection::__attach_module_with_provenance(transport, provenance) }
    } else {
        Connection::attach(transport)
    }
}

struct RuntimeState {
    owner: Option<ExecutionOwner>,
    inbound: StdMutex<Option<mpsc::Sender<IncomingFrame>>>,
    host: HostCalls,
    runtime: StdMutex<Option<tokio::runtime::Runtime>>,
    reinitialize: Arc<StdMutex<Option<Reinitialize>>>,
}

type Reinitialize = Box<dyn Fn(*const u8, usize) -> i32 + Send + Sync>;
async fn dispose_failed_setup_callback(
    callback: Arc<StdMutex<Option<Reinitialize>>>,
) -> Result<()> {
    // Reinitialization holds exclusion while driving setup with block_on.
    // Waiting for it must leave runtime workers free to advance that future
    // and its timer. Observe completion before publishing the failure.
    tokio::task::spawn_blocking(move || {
        callback
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    })
    .await
    .map_err(|_| Error::failed("module failed-setup callback disposal panicked"))
}
type ReinitializeFactory = Box<
    dyn FnOnce(tokio::runtime::Handle, Arc<StdMutex<Option<Connection>>>) -> Reinitialize + Send,
>;

unsafe extern "C" fn deliver(ctx: *mut c_void, ptr: *const u8, len: usize) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() || ptr.is_null() || len > MAX_FRAME_LEN {
            return TB_BAD_ARGUMENT;
        }
        let state = unsafe { &*(ctx.cast::<RuntimeState>()) };
        let _caller = state.owner.as_ref().map(ExecutionOwner::enter);
        let brokered = state.host.is_broker_routed();
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        let sender = state.inbound.lock().expect("module inbound lock").clone();
        match sender {
            Some(sender) => match sender.try_send((bytes, brokered)) {
                Ok(()) => TB_OK,
                Err(mpsc::error::TrySendError::Full(_)) => TB_BACKPRESSURE,
                Err(mpsc::error::TrySendError::Closed(_)) => TB_CLOSED,
            },
            None => TB_CLOSED,
        }
    })) {
        Ok(code) => code,
        Err(_) => TB_PANICKED,
    }
}

unsafe extern "C" fn shutdown(ctx: *mut c_void, deadline_ms: u64) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() {
            return TB_BAD_ARGUMENT;
        }
        let state = unsafe { &*(ctx.cast::<RuntimeState>()) };
        let _caller = state.owner.as_ref().map(ExecutionOwner::enter);
        // An in-flight reinitializer finishes before disposal. Once the slot
        // is empty, late calls see a tombstone. Release the lock before joining
        // runtime tasks, whose failed-setup cleanup also uses this slot.
        let mut callback = state
            .reinitialize
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner) = &state.owner {
            owner.stop();
        }
        callback.take();
        drop(callback);
        state.inbound.lock().expect("module inbound lock").take();
        match state.runtime.lock().expect("module runtime lock").take() {
            Some(runtime) => {
                runtime.shutdown_timeout(Duration::from_millis(deadline_ms));
                TB_OK
            }
            None => TB_CLOSED,
        }
    })) {
        Ok(code) => code,
        Err(_) => TB_PANICKED,
    }
}

unsafe extern "C" fn reinitialize(ctx: *mut c_void, ptr: *const u8, len: usize) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() || len > 1024 * 1024 || (ptr.is_null() && len != 0) {
            return TB_BAD_ARGUMENT;
        }
        let state = unsafe { &*(ctx.cast::<RuntimeState>()) };
        let callback = state
            .reinitialize
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(reinitialize) = callback.as_ref() else {
            return TB_CLOSED;
        };
        reinitialize(ptr, len)
    })) {
        Ok(code) => code,
        Err(_) => TB_PANICKED,
    }
}

unsafe fn write_module_vtable(
    out: *mut TbModuleVtable,
    out_capacity: u32,
    state: *mut c_void,
    supports_reinitialize: bool,
) {
    if out_capacity >= size_of::<TbModuleVtable>() as u32 {
        unsafe {
            out.write(TbModuleVtable {
                size: if supports_reinitialize {
                    size_of::<TbModuleVtable>() as u32
                } else {
                    TB_MODULE_VTABLE_BASE_SIZE
                },
                _reserved: 0,
                module_ctx: state,
                deliver,
                shutdown,
                reinitialize: Some(reinitialize),
            });
        }
    } else {
        unsafe {
            out.cast::<TbModuleVtableV1Prefix>()
                .write(TbModuleVtableV1Prefix {
                    size: TB_MODULE_VTABLE_BASE_SIZE,
                    _reserved: 0,
                    module_ctx: state,
                    deliver,
                    shutdown,
                });
        }
    }
}

/// Initialize the module runtime and start its async setup function.
///
/// This is public only for [`module_export!`] expansions. Module authors call
/// the macro, not this function directly.
///
/// # Safety
/// The host vtable must meet [`TbHostVtable::read_compatible`]'s requirements.
/// A non-null `out` must be aligned and writable for its declared capacity,
/// with a readable size field; writes are bounded to that capacity. Borrowed
/// configuration bytes must remain readable during initialization.
/// In particular an affirmative routing callback is a trusted host assertion
/// that only real broker deliveries enter `deliver`, not an arbitrary frame
/// injection capability. Artifact attestation does not establish this fact.
#[doc(hidden)]
pub unsafe fn start_module<F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
) -> i32
where
    F: FnOnce(Connection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let Some((_, compatible_host)) =
        (unsafe { validate_start_arguments(host, out, worker_threads) })
    else {
        return TB_BAD_ARGUMENT;
    };
    let owner = Some(ExecutionOwner::new(HostCalls(compatible_host)));
    unsafe {
        start_module_runtime(
            host,
            out,
            worker_threads,
            detach_on_panic,
            setup,
            None,
            owner,
        )
    }
}

/// Start a linked module without replacing the host's process-global hooks.
///
/// # Safety
/// The caller must uphold the host, output, configuration, and routing contracts
/// of [`start_module`].
#[doc(hidden)]
pub unsafe fn start_linked_module<F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
) -> i32
where
    F: FnOnce(Connection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    unsafe {
        start_module_runtime(
            host,
            out,
            worker_threads,
            detach_on_panic,
            setup,
            None,
            None,
        )
    }
}

// Validate the frozen ABI prefix before configuration parsing or a factory can
// allocate state. Never read or write beyond a caller's declared allocation.
unsafe fn validate_start_arguments(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
) -> Option<(u32, TbHostVtable)> {
    if host.is_null() || out.is_null() || worker_threads == 0 {
        return None;
    }
    let out_capacity = unsafe { out.cast::<u32>().read() };
    if out_capacity < TB_MODULE_VTABLE_BASE_SIZE {
        return None;
    }
    let host = unsafe { TbHostVtable::read_compatible(host) }?;
    Some((out_capacity, host))
}

unsafe fn start_module_runtime<F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
    reinitialize_factory: Option<ReinitializeFactory>,
    owner: Option<ExecutionOwner>,
) -> i32
where
    F: FnOnce(Connection) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    match catch_unwind(AssertUnwindSafe(|| {
        let Some((out_capacity, host)) =
            (unsafe { validate_start_arguments(host, out, worker_threads) })
        else {
            return TB_BAD_ARGUMENT;
        };
        let host = HostCalls(host);

        if owner.is_some() {
            ExecutionOwner::install_panic_hook();
        }
        let _caller = owner.as_ref().map(ExecutionOwner::enter);
        let panic_location = owner
            .as_ref()
            .map(|owner| owner.panic_location.clone())
            .unwrap_or_else(|| Arc::new(StdMutex::new(None)));
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        if let Some(owner) = &owner {
            owner.configure(&mut builder);
        }

        let runtime = match builder.worker_threads(worker_threads).enable_all().build() {
            Ok(runtime) => runtime,
            Err(_) => return TB_CLOSED,
        };
        let (inbound_tx, inbound_rx) = mpsc::channel(MODULE_QUEUE_CAPACITY);
        let transport = Arc::new(ModuleTransport {
            host,
            inbound: Mutex::new(inbound_rx),
            provenance: Arc::new(AtomicBool::new(false)),
            detach_on_panic,
        });
        let connection_slot = Arc::new(StdMutex::new(None));
        let task_connection_slot = connection_slot.clone();

        let reinitializer = reinitialize_factory.map(|factory| {
            let callback = factory(runtime.handle().clone(), connection_slot);
            let owner = owner.clone();
            Box::new(move |ptr, len| {
                let _caller = owner.as_ref().map(ExecutionOwner::enter);
                callback(ptr, len)
            }) as Reinitialize
        });
        let supports_reinitialize = reinitializer.is_some();
        let reinitializer = Arc::new(StdMutex::new(reinitializer));
        let failed_setup_callback = reinitializer.clone();
        let setup_task = runtime.spawn(async move {
            let connection = module_connection(transport);
            match connection.handshake().await.map(|()| connection) {
                Ok(connection) => {
                    connection.__set_panic_handler(std::sync::Arc::new(move || {
                        let location = panic_location
                            .lock()
                            .expect("panic location lock")
                            .take()
                            .unwrap_or_else(|| "an unknown location".to_string());
                        Error::MethodFailed {
                            name: MODULE_PANIC_ERROR.to_string(),
                            message: format!("a module method panicked at {location}"),
                        }
                    }));
                    match setup(connection.clone()).await {
                        Ok(()) => {
                            *task_connection_slot.lock().expect("module connection lock") =
                                Some(connection.clone());
                            host.ready();
                            // Keep the connection (and therefore the served
                            // object tree and transport) alive until shutdown
                            // stops this runtime. Setup returning means ready,
                            // not that the module has finished serving.
                            std::future::pending::<()>().await;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        });
        runtime.spawn(async move {
            let outcome = setup_task.await;
            if !matches!(outcome, Ok(Ok(()))) {
                // A terminal setup failure may never receive ABI shutdown.
                // Dispose its callback under the same lifecycle exclusion.
                if dispose_failed_setup_callback(failed_setup_callback)
                    .await
                    .is_err()
                {
                    tracing::error!("tinybus module failed-setup callback disposal failed");
                }
                match outcome {
                    Ok(Err(error)) => tracing::error!(error = %error, "tinybus module failed"),
                    Err(_) => tracing::error!("tinybus module setup task failed"),
                    Ok(Ok(())) => unreachable!(),
                }
                host.fault();
            }
        });

        let state = Box::new(RuntimeState {
            owner,
            host,
            inbound: StdMutex::new(Some(inbound_tx)),
            runtime: StdMutex::new(Some(runtime)),
            reinitialize: reinitializer,
        });
        let state = Box::into_raw(state).cast::<c_void>();
        unsafe { write_module_vtable(out, out_capacity, state, supports_reinitialize) };
        TB_OK
    })) {
        Ok(code) => code,
        Err(_) => TB_PANICKED,
    }
}

unsafe fn parse_config<C: serde::de::DeserializeOwned>(
    host: *const TbHostVtable,
) -> std::result::Result<C, i32> {
    if host.is_null() {
        return Err(TB_BAD_ARGUMENT);
    }
    let host_ref = unsafe { TbHostVtable::read_compatible(host) }.ok_or(TB_BAD_ARGUMENT)?;
    if host_ref.config.len > 1024 * 1024
        || (host_ref.config.ptr.is_null() && host_ref.config.len != 0)
    {
        return Err(TB_BAD_ARGUMENT);
    }
    let bytes = if host_ref.config.len == 0 {
        b"{}".as_slice()
    } else {
        unsafe { std::slice::from_raw_parts(host_ref.config.ptr, host_ref.config.len) }
    };
    serde_json::from_slice::<C>(bytes).map_err(|_| TB_BAD_ARGUMENT)
}

/// Initialize a configured module without enabling live reconfiguration.
///
/// This retains the original `FnOnce` contract for direct SDK callers. The
/// export macro uses [`start_reconfigurable_module`] because a named setup
/// function can safely be called again.
///
/// # Safety
/// The caller must uphold the host, output, configuration, and routing contracts
/// of [`start_module`].
#[doc(hidden)]
pub unsafe fn start_module_with_config<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: FnOnce(Connection, C) -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let Some((_, compatible_host)) =
        (unsafe { validate_start_arguments(host, out, worker_threads) })
    else {
        return TB_BAD_ARGUMENT;
    };
    let caller_owner = Some(ExecutionOwner::new(HostCalls(compatible_host)));
    if caller_owner.is_some() {
        ExecutionOwner::install_panic_hook();
    }
    let _caller = caller_owner.as_ref().map(ExecutionOwner::enter);
    let parsed = catch_unwind(AssertUnwindSafe(|| unsafe { parse_config::<C>(host) }));
    let config = match parsed {
        Ok(Ok(config)) => config,
        Ok(Err(code)) => return code,
        Err(_) => return TB_PANICKED,
    };
    unsafe {
        start_module_runtime(
            host,
            out,
            worker_threads,
            detach_on_panic,
            move |connection| setup(connection, config),
            None,
            caller_owner,
        )
    }
}

/// Initialize a configured module whose setup function may be called again.
///
/// # Safety
/// The caller must uphold the host, output, configuration, and routing contracts
/// of [`start_module`].
#[doc(hidden)]
pub unsafe fn start_reconfigurable_module<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    unsafe {
        start_reconfigurable_module_mode(host, out, worker_threads, detach_on_panic, setup, false)
    }
}

/// Start a configurable linked module without process-global hooks.
///
/// # Safety
/// The caller must uphold the host, output, configuration, and routing contracts
/// of [`start_module`].
#[doc(hidden)]
pub unsafe fn start_linked_reconfigurable_module<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    unsafe {
        start_reconfigurable_module_mode(host, out, worker_threads, detach_on_panic, setup, true)
    }
}

unsafe fn start_reconfigurable_module_mode<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    setup: F,
    linked: bool,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    unsafe {
        start_reconfigurable_module_factory_mode(
            host,
            out,
            worker_threads,
            detach_on_panic,
            || setup,
            linked,
        )
    }
}

/// Initialize one configured instance with fresh captured setup state.
///
/// # Safety
/// Non-null pointers must address readable host ABI prefixes and writable
/// output storage matching their declared sizes. Host callbacks and context
/// must remain valid until runtime shutdown. Null pointers and invalid sizes
/// or worker counts are rejected before configuration or factory evaluation.
#[doc(hidden)]
pub unsafe fn start_reconfigurable_module_with_factory<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    factory: impl FnOnce() -> F,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    unsafe {
        start_reconfigurable_module_factory_mode(
            host,
            out,
            worker_threads,
            detach_on_panic,
            || shared_factory_setup(factory),
            false,
        )
    }
}

/// Initialize one linked configured instance with fresh captured setup state.
///
/// # Safety
/// Non-null pointers must address readable host ABI prefixes and writable
/// output storage matching their declared sizes. Host callbacks and context
/// must remain valid until runtime shutdown. Null pointers and invalid sizes
/// or worker counts are rejected before configuration or factory evaluation.
#[doc(hidden)]
pub unsafe fn start_linked_reconfigurable_module_with_factory<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    factory: impl FnOnce() -> F,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    unsafe {
        start_reconfigurable_module_factory_mode(
            host,
            out,
            worker_threads,
            detach_on_panic,
            || shared_factory_setup(factory),
            true,
        )
    }
}

// The factory callback itself is shared, rather than cloned: a user's Clone
// may deep-copy interior state and detach initial setup from reinitialization.
fn shared_factory_setup<C, F, Fut>(
    factory: impl FnOnce() -> F,
) -> impl Fn(Connection, C) -> Fut + Clone + Send + Sync + 'static
where
    F: Fn(Connection, C) -> Fut + Send + Sync + 'static,
{
    let setup = Arc::new(factory());
    move |connection, config| setup(connection, config)
}

unsafe fn start_reconfigurable_module_factory_mode<C, F, Fut>(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
    worker_threads: usize,
    detach_on_panic: bool,
    factory: impl FnOnce() -> F,
    linked: bool,
) -> i32
where
    C: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(Connection, C) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let Some((_, compatible_host)) =
        (unsafe { validate_start_arguments(host, out, worker_threads) })
    else {
        return TB_BAD_ARGUMENT;
    };
    let caller_owner = (!linked).then(|| ExecutionOwner::new(HostCalls(compatible_host)));
    if caller_owner.is_some() {
        ExecutionOwner::install_panic_hook();
    }
    let _caller = caller_owner.as_ref().map(ExecutionOwner::enter);
    let parsed = catch_unwind(AssertUnwindSafe(|| unsafe { parse_config::<C>(host) }));
    let config = match parsed {
        Ok(Ok(config)) => config,
        Ok(Err(code)) => return code,
        Err(_) => return TB_PANICKED,
    };
    let setup = match catch_unwind(AssertUnwindSafe(factory)) {
        Ok(setup) => setup,
        Err(_) => return TB_PANICKED,
    };
    let initial_setup = setup.clone();
    let reinitialize_factory: ReinitializeFactory = Box::new(move |handle, connection_slot| {
        let reinitialize_lock = StdMutex::new(());
        Box::new(move |ptr, len| {
            // Setup mutates the module's live object tree. Serializing retries
            // keeps two operators from interleaving partial configurations.
            let _guard = reinitialize_lock.lock().expect("module reinitialize lock");
            let bytes = if len == 0 {
                b"{}".as_slice()
            } else {
                // SAFETY: the ABI callback validates this borrowed slice and
                // keeps it alive for the duration of this call.
                unsafe { std::slice::from_raw_parts(ptr, len) }
            };
            let Ok(config) = serde_json::from_slice::<C>(bytes) else {
                return TB_BAD_ARGUMENT;
            };
            let Some(connection) = connection_slot
                .lock()
                .expect("module connection lock")
                .clone()
            else {
                return TB_CLOSED;
            };
            match handle.block_on(async {
                tokio::time::timeout(MODULE_REINITIALIZE_DEADLINE, setup(connection, config)).await
            }) {
                Ok(Ok(())) => TB_OK,
                Ok(Err(_)) => TB_CLOSED,
                Err(_) => TB_TIMEOUT,
            }
        })
    });
    unsafe {
        start_module_runtime(
            host,
            out,
            worker_threads,
            detach_on_panic,
            move |connection| initial_setup(connection, config),
            Some(reinitialize_factory),
            caller_owner,
        )
    }
}

/// Export a tinybus module's descriptor, manifest and initialization entrypoint.
///
/// ```ignore
/// tinybus_module::module_export! { setup = setup, worker_threads = 1 }
/// ```
///
/// Configured modules may use `setup_factory = factory` in place of `setup`.
/// After valid configuration is parsed, the safe no-argument factory runs once
/// per ABI initialization and returns a
/// `Fn(Connection, Config) -> Future<Output = Result<()>>`. The callback is shared
/// by reference for initial setup and reinitialization, regardless of its Clone
/// behavior. Its captured
/// state is reused for that instance's reinitializations, allowing admission to
/// remain bound without process-global state. Separate instances receive fresh
/// state. A factory panic returns `TB_PANICKED` without unwinding into the host.
/// Existing `setup = function` callbacks retain their behavior.
#[macro_export]
macro_rules! module_export {
    (@linked) => {
        /// Entry points and parsed manifest for a host that links this module.
        ///
        /// # Errors
        /// Returns an error if the generated manifest is invalid.
        pub fn linked_module() -> ::tinybus::Result<::tinybus::module::LinkedModule> {
            unsafe {
                ::tinybus::module::LinkedModule::from_exports(
                    &TINYBUS_MODULE_ABI_V1,
                    tinybus_module_manifest_v1,
                    tinybus_module_init_v1,
                )
            }
        }
    };
    (@common
        export = {$($export:tt)*},
        worker_threads = $threads:expr,
        provides = [$($provides:literal),* $(,)?],
        methods = [$($methods:literal),* $(,)?],
        signals = [$($signals:literal),* $(,)?],
        requires = [$($requires:literal),* $(,)?],
        optional = [$($optional:literal),* $(,)?],
        lazy = $lazy:expr $(,)?
    ) => {
        $($export)*
        pub static TINYBUS_MODULE_ABI_V1: ::tinybus::module::abi::TbAbiDescriptor =
            ::tinybus::module::abi::TbAbiDescriptor::current(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            );

        $($export)*
        pub extern "C" fn tinybus_module_manifest_v1() -> ::tinybus::module::abi::TbSlice {
            static MANIFEST_BYTES: ::std::sync::OnceLock<::std::vec::Vec<u8>> =
                ::std::sync::OnceLock::new();
            $crate::manifest_slice_in(&MANIFEST_BYTES, $crate::ManifestDeclaration {
                name: env!("CARGO_PKG_NAME"),
                version: env!("CARGO_PKG_VERSION"),
                provides: &[$($provides),*],
                methods: &[$($methods),*],
                signals: &[$($signals),*],
                requires: &[$($requires),*],
                optional: &[$($optional),*],
                lazy: $lazy,
                worker_threads: $threads as u32,
            })
        }
    };
    (@configured
        export = {$($export:tt)*},
        start = $start:path,
        setup = $setup:path,
        config = $config:ty,
        worker_threads = $threads:expr,
        provides = [$($provides:literal),* $(,)?],
        methods = [$($methods:literal),* $(,)?],
        signals = [$($signals:literal),* $(,)?],
        requires = [$($requires:literal),* $(,)?],
        optional = [$($optional:literal),* $(,)?],
        lazy = $lazy:expr $(,)?
    ) => {
        $crate::module_export! {
            @common
            export = {$($export)*},
            worker_threads = $threads,
            provides = [$($provides),*],
            methods = [$($methods),*],
            signals = [$($signals),*],
            requires = [$($requires),*],
            optional = [$($optional),*],
            lazy = $lazy,
        }

        $($export)*
        pub unsafe extern "C" fn tinybus_module_init_v1(
            host: *const ::tinybus::module::abi::TbHostVtable,
            out: *mut ::tinybus::module::abi::TbModuleVtable,
        ) -> i32 {
            use $start as start;
            unsafe {
                start::<$config, _, _>(
                    host,
                    out,
                    $threads,
                    true,
                    $setup,
                )
            }
        }
    };
    (setup = $setup:path, worker_threads = $threads:expr $(,)?) => {
        $crate::module_export! {
            setup = $setup,
            worker_threads = $threads,
            provides = [],
            methods = [],
            signals = [],
            requires = [],
            optional = [],
            lazy = false,
        }
    };
    (@plain
        export = {$($export:tt)*},
        start = $start:path,
        setup = $setup:path,
        worker_threads = $threads:expr,
        provides = [$($provides:literal),* $(,)?],
        methods = [$($methods:literal),* $(,)?],
        signals = [$($signals:literal),* $(,)?],
        requires = [$($requires:literal),* $(,)?],
        optional = [$($optional:literal),* $(,)?],
        lazy = $lazy:expr $(,)?
    ) => {
        $crate::module_export! {
            @common
            export = {$($export)*},
            worker_threads = $threads,
            provides = [$($provides),*],
            methods = [$($methods),*],
            signals = [$($signals),*],
            requires = [$($requires),*],
            optional = [$($optional),*],
            lazy = $lazy,
        }

        $($export)*
        pub unsafe extern "C" fn tinybus_module_init_v1(
            host: *const ::tinybus::module::abi::TbHostVtable,
            out: *mut ::tinybus::module::abi::TbModuleVtable,
        ) -> i32 {
            unsafe { $start(host, out, $threads, true, $setup) }
        }
    };
    (
        setup_factory = $factory:path,
        config = $config:ty,
        $($rest:tt)*
    ) => {
        $crate::module_export! {
            @configured export = {#[unsafe(no_mangle)]},
            start = $crate::start_reconfigurable_module_with_factory,
            setup = $factory,
            config = $config,
            $($rest)*
        }
    };
    (
        setup = $setup:path,
        config = $config:ty,
        $($rest:tt)*
    ) => {
        $crate::module_export! {
            @configured export = {#[unsafe(no_mangle)]},
            start = $crate::start_reconfigurable_module,
            setup = $setup,
            config = $config,
            $($rest)*
        }
    };
    (setup = $setup:path, $($rest:tt)*) => {
        $crate::module_export! {
            @plain export = {#[unsafe(no_mangle)]},
            start = $crate::start_module,
            setup = $setup,
            $($rest)*
        }
    };
}

/// Generate Rust-addressable entry points for linking several modules into a
/// single executable. The entry points have no shared C linker symbol names.
/// Both configured `setup` and per-instance `setup_factory` forms are supported.
#[macro_export]
macro_rules! module_export_static {
    (setup_factory = $factory:path, config = $config:ty, $($rest:tt)*) => {
        $crate::module_export! {
            @configured export = {},
            start = $crate::start_linked_reconfigurable_module_with_factory,
            setup = $factory,
            config = $config,
            $($rest)*
        }
        $crate::module_export! { @linked }
    };
    (setup = $setup:path, config = $config:ty, $($rest:tt)*) => {
        $crate::module_export! {
            @configured export = {},
            start = $crate::start_linked_reconfigurable_module,
            setup = $setup,
            config = $config,
            $($rest)*
        }
        $crate::module_export! { @linked }
    };
    (setup = $setup:path, $($rest:tt)*) => {
        $crate::module_export! {
            @plain export = {},
            start = $crate::start_linked_module,
            setup = $setup,
            $($rest)*
        }
        $crate::module_export! { @linked }
    };
}

/// Select the dynamic or linked export from one declaration.
///
/// The consuming crate declares a `static-link` feature. Its default build
/// retains the C exports, while the linked build uses Rust-addressable entry
/// points and the linked runtime mode.
#[macro_export]
macro_rules! module_export_optional_static {
    ($($declaration:tt)*) => {
        #[cfg(not(feature = "static-link"))]
        $crate::module_export! { $($declaration)* }
        #[cfg(feature = "static-link")]
        $crate::module_export_static! { $($declaration)* }
    };
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "static_link_tests.rs"]
mod static_link_tests;

#[cfg(test)]
#[path = "lib_setup_factory_tests.rs"]
mod setup_factory_test;

#[cfg(test)]
#[path = "lib_lifecycle_tests.rs"]
mod lifecycle_test;
