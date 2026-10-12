use super::*;
use std::sync::atomic::AtomicI32;

use crate::{BusName, InterfaceName, MemberName, ObjectPath};

static DELIVERY_CODE: AtomicI32 = AtomicI32::new(TB_OK);
static DELIVERIES: AtomicUsize = AtomicUsize::new(0);
static DELIVERY_BLOCKED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_CODE: AtomicI32 = AtomicI32::new(TB_OK);
static SHUTDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);
static SHUTDOWN_BLOCKED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_ENTERED: AtomicBool = AtomicBool::new(false);
static REINITIALIZE_CODE: AtomicI32 = AtomicI32::new(TB_OK);
static REINITIALIZE_BLOCKED: AtomicBool = AtomicBool::new(false);
static REINITIALIZE_ENTERED: AtomicBool = AtomicBool::new(false);
static VTABLE_TEST_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

unsafe extern "C" fn deliver(_: *mut c_void, _: *const u8, _: usize) -> i32 {
    DELIVERIES.fetch_add(1, Ordering::AcqRel);
    while DELIVERY_BLOCKED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    DELIVERY_CODE.load(Ordering::Acquire)
}

unsafe extern "C" fn shutdown(_: *mut c_void, _: u64) -> i32 {
    SHUTDOWN_CALLS.fetch_add(1, Ordering::AcqRel);
    SHUTDOWN_ENTERED.store(true, Ordering::Release);
    while SHUTDOWN_BLOCKED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    SHUTDOWN_CODE.load(Ordering::Acquire)
}

unsafe extern "C" fn reinitialize(_: *mut c_void, _: *const u8, _: usize) -> i32 {
    REINITIALIZE_ENTERED.store(true, Ordering::Release);
    while REINITIALIZE_BLOCKED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    REINITIALIZE_CODE.load(Ordering::Acquire)
}

unsafe extern "C" fn initialize_ok(_: *const TbHostVtable, out: *mut TbModuleVtable) -> i32 {
    unsafe {
        *out = TbModuleVtable {
            size: size_of::<TbModuleVtable>() as u32,
            _reserved: 0,
            module_ctx: std::ptr::dangling_mut(),
            deliver,
            shutdown,
            reinitialize: Some(reinitialize),
        };
    }
    TB_OK
}

unsafe extern "C" fn initialize_fails(_: *const TbHostVtable, _: *mut TbModuleVtable) -> i32 {
    TB_BAD_ARGUMENT
}

fn call() -> Message {
    Message::method_call(
        "org.example.Module".parse::<BusName>().unwrap(),
        "/org/example/Module".parse::<ObjectPath>().unwrap(),
        "org.example.Module".parse::<InterfaceName>().unwrap(),
        "Call".parse::<MemberName>().unwrap(),
        serde_json::Value::Array(Vec::new()),
    )
}

struct CaptureLevel(Arc<AtomicBool>);

impl tracing::Subscriber for CaptureLevel {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::ERROR {
            self.0.store(true, Ordering::Release);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn a_module_log_line_reaches_the_hosts_subscriber_with_its_level() {
    let (_transport, host) = ModuleTransport::new("logger".to_string(), Vec::new());
    let observed = Arc::new(AtomicBool::new(false));
    let bytes = b"module log line";
    tracing::subscriber::with_default(CaptureLevel(observed.clone()), || unsafe {
        (host.log)(host.host_ctx, 1, bytes.as_ptr(), bytes.len());
    });
    assert!(observed.load(Ordering::Acquire));
}

#[test]
fn incomplete_module_vtables_are_rejected_and_shutdown_reports_closed() {
    let (transport, _) = ModuleTransport::new("broken".to_string(), Vec::new());
    let incomplete = TbModuleVtable::default();
    assert!(transport.initialize(incomplete).is_err());
    assert_eq!(transport.shutdown_sync(Duration::ZERO), TB_CLOSED);
}

#[tokio::test]
async fn an_older_vtable_loads_but_reports_reinitialization_as_unsupported() {
    let (transport, _) = ModuleTransport::new("older".to_string(), Vec::new());
    let mut older = TbModuleVtable {
        module_ctx: std::ptr::dangling_mut(),
        deliver,
        shutdown,
        reinitialize: Some(reinitialize),
        ..TbModuleVtable::default()
    };
    older.size = TB_MODULE_VTABLE_BASE_SIZE;
    transport.initialize(older).unwrap();
    let error = transport
        .reinitialize(crate::Secret::new(
            serde_json::to_vec(&serde_json::json!({ "secret": "never printed" })).unwrap(),
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("does not support"));
    assert!(!error.to_string().contains("never printed"));

    let (missing_callback, _) = ModuleTransport::new("missing-callback".to_string(), Vec::new());
    assert!(
        missing_callback
            .initialize(TbModuleVtable {
                module_ctx: std::ptr::dangling_mut(),
                deliver,
                shutdown,
                reinitialize: None,
                ..TbModuleVtable::default()
            })
            .is_err()
    );
}

#[tokio::test]
async fn reinitialization_maps_callback_failures_without_exposing_configuration() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let (uninitialized, _) = ModuleTransport::new("missing".to_string(), Vec::new());
    assert!(
        uninitialized
            .reinitialize(crate::Secret::new(
                serde_json::to_vec(&serde_json::json!({})).unwrap()
            ))
            .await
            .unwrap_err()
            .to_string()
            .contains("not initialized")
    );

    let (transport, _) = ModuleTransport::new("configured".to_string(), Vec::new());
    let mut module = TbModuleVtable::default();
    unsafe { initialize_ok(std::ptr::null(), &mut module) };
    transport.initialize(module).unwrap();
    for (code, expected) in [
        (TB_BAD_ARGUMENT, "configuration is invalid"),
        (TB_CLOSED, "reinitialization failed"),
        (TB_PANICKED, "reinitialization panicked"),
        (77, "reinitialization failed"),
    ] {
        REINITIALIZE_CODE.store(code, Ordering::Release);
        let error = transport
            .reinitialize(crate::Secret::new(
                serde_json::to_vec(&serde_json::json!({ "secret": "never printed" })).unwrap(),
            ))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("never printed"));
    }
    REINITIALIZE_CODE.store(TB_OK, Ordering::Release);
    transport
        .reinitialize(crate::Secret::new(
            serde_json::to_vec(&serde_json::json!({ "replacement": true })).unwrap(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_running_reinitialization_excludes_another_reinitialization_and_shutdown() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    REINITIALIZE_CODE.store(TB_OK, Ordering::Release);
    REINITIALIZE_BLOCKED.store(true, Ordering::Release);
    REINITIALIZE_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    let (transport, _) = ModuleTransport::new("configured".to_string(), Vec::new());
    let mut module = TbModuleVtable::default();
    unsafe { initialize_ok(std::ptr::null(), &mut module) };
    transport.initialize(module).unwrap();

    let first_transport = transport.clone();
    let first = tokio::spawn(async move {
        first_transport
            .reinitialize(crate::Secret::new(
                serde_json::to_vec(&serde_json::json!({ "generation": 1 })).unwrap(),
            ))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !REINITIALIZE_ENTERED.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            transport.reinitialize(crate::Secret::new(
                serde_json::to_vec(&serde_json::json!({ "generation": 2 })).unwrap()
            )),
        )
        .await
        .is_err()
    );
    assert!(matches!(
        transport.clone().stop(Duration::from_millis(20)).await,
        Err(StopError::NotStarted(_))
    ));
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 0);

    REINITIALIZE_BLOCKED.store(false, Ordering::Release);
    first.await.unwrap().unwrap();
    assert_eq!(
        transport
            .clone()
            .stop(Duration::from_secs(1))
            .await
            .unwrap(),
        TB_OK
    );
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn a_shutdown_task_timeout_is_distinguished_from_a_lifecycle_lock_timeout() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    let (transport, _) = ModuleTransport::new("configured".to_string(), Vec::new());
    let mut module = TbModuleVtable::default();
    unsafe { initialize_ok(std::ptr::null(), &mut module) };
    transport.initialize(module).unwrap();

    assert!(matches!(
        transport.clone().stop(Duration::from_millis(20)).await,
        Err(StopError::Started(_))
    ));
    assert!(SHUTDOWN_ENTERED.load(Ordering::Acquire));
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    wait_physical_cleanup(&transport).await;
}

#[tokio::test]
async fn an_uninitialized_module_answers_calls_and_rejects_signals() {
    let (transport, _) = ModuleTransport::new("missing".to_string(), Vec::new());
    transport.send(call()).await.unwrap();
    let reply = transport.recv().await.unwrap().unwrap();
    assert_eq!(reply.header.kind, MessageKind::Error);
    assert!(transport.init_failed());
    assert!(transport.is_faulted());

    let (transport, _) = ModuleTransport::new("missing".to_string(), Vec::new());
    let signal = Message::signal(
        "/org/example/Module".parse().unwrap(),
        "org.example.Module".parse().unwrap(),
        "Changed".parse().unwrap(),
        serde_json::Value::Null,
    );
    assert!(matches!(
        transport.send(signal).await,
        Err(Error::ConnectionClosed)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_initialization_waits_for_ready_then_delivers_pending_calls() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    DELIVERY_CODE.store(TB_OK, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("deferred".to_string(), br#"{"key":1}"#.to_vec());
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.unwrap();
    transport.wait_initializing().await;
    assert_eq!(transport.inflight(), 1);
    unsafe { (host.ready)(host.host_ctx) };
    tokio::time::timeout(Duration::from_secs(1), async {
        while DELIVERIES.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(transport.is_ready());
    assert!(transport.context.config.lock().unwrap().is_empty());
    assert_eq!(transport.stop_sync(Duration::from_millis(1)), TB_OK);
}

#[tokio::test]
async fn a_failed_initializer_is_reported_to_callers() {
    let (transport, host) =
        ModuleTransport::new("fails-init".to_string(), br#"{"secret":"value"}"#.to_vec());
    transport.defer_initialize(initialize_fails, host);
    transport.send(call()).await.unwrap();
    assert_eq!(
        transport.recv().await.unwrap().unwrap().header.kind,
        MessageKind::Error
    );
    assert!(transport.init_failed());
    assert!(transport.context.config.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_pending_call_is_failed_when_the_module_closes_its_delivery_queue() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    DELIVERY_CODE.store(TB_CLOSED, Ordering::Release);
    let (transport, host) = ModuleTransport::new("closed".to_string(), Vec::new());
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.unwrap();
    unsafe { (host.ready)(host.host_ctx) };
    let reply = tokio::time::timeout(Duration::from_secs(1), transport.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.kind, MessageKind::Error);
    assert!(transport.is_faulted());
    DELIVERY_CODE.store(TB_OK, Ordering::Release);
}

#[tokio::test]
async fn host_callbacks_accept_messages_and_make_the_transport_closed_on_fault() {
    let (transport, host) = ModuleTransport::new("callbacks".to_string(), Vec::new());
    let outgoing = serde_json::to_vec(&call()).unwrap();
    let host_ctx = host.host_ctx as usize;
    let send = host.send;
    let outbound = outgoing.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || unsafe {
            send(host_ctx as *mut c_void, outbound.as_ptr(), outbound.len())
        })
        .await
        .unwrap(),
        TB_OK
    );
    assert_eq!(transport.recv().await.unwrap().unwrap(), call());
    assert_eq!(
        unsafe { (host.send)(host.host_ctx, std::ptr::null(), 1) },
        TB_BAD_ARGUMENT
    );
    unsafe { (host.fault)(host.host_ctx, std::ptr::null(), 0) };
    assert!(transport.is_faulted());
    assert!(transport.recv().await.unwrap().is_none());
    assert_eq!(
        unsafe { (host.send)(host.host_ctx, outgoing.as_ptr(), outgoing.len()) },
        TB_CLOSED
    );
}

#[test]
fn stop_errors_keep_the_underlying_safe_error_for_both_lifecycle_phases() {
    for error in [
        StopError::NotStarted(Error::failed("before spawn")),
        StopError::Started(Error::failed("after spawn")),
    ] {
        assert!(matches!(Error::from(error), Error::MethodFailed { .. }));
    }
}

#[test]
fn a_module_log_line_is_accepted_at_every_level_and_with_a_hostile_length() {
    let (_transport, host) = ModuleTransport::new("levels".to_string(), Vec::new());
    let bytes = vec![b'x'; 5000];
    for level in 0..=5 {
        unsafe { (host.log)(host.host_ctx, level, bytes.as_ptr(), bytes.len()) };
    }
}

#[test]
fn every_host_callback_refuses_null_arguments_without_crashing() {
    let (_transport, host) = ModuleTransport::new("nulls".to_string(), Vec::new());
    let byte = 0u8;
    unsafe {
        assert_eq!((host.send)(std::ptr::null_mut(), &byte, 1), TB_BAD_ARGUMENT);
        assert_eq!(
            (host.send)(host.host_ctx, std::ptr::null(), 1),
            TB_BAD_ARGUMENT
        );
        assert_eq!(
            (host.send)(host.host_ctx, &byte, MAX_FRAME_LEN + 1),
            TB_BAD_ARGUMENT
        );
        (host.wake)(std::ptr::null_mut());
        (host.log)(std::ptr::null_mut(), 1, &byte, 1);
        (host.log)(host.host_ctx, 1, std::ptr::null(), 1);
        (host.fault)(std::ptr::null_mut(), std::ptr::null(), 0);
        (host.ready)(std::ptr::null_mut());
    }
}

#[test]
fn a_module_that_faults_is_detached_and_can_no_longer_send() {
    let (transport, host) = ModuleTransport::new("faulty".to_string(), Vec::new());
    let byte = 0u8;
    unsafe {
        (host.ready)(host.host_ctx);
        (host.wake)(host.host_ctx);
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
        assert_eq!((host.send)(host.host_ctx, &byte, 1), TB_CLOSED);
    }
    assert!(transport.is_faulted());
}

#[tokio::test]
async fn a_reinitialization_that_times_out_reports_a_timeout_without_the_configuration() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let (transport, _) = ModuleTransport::new("timeout".to_string(), Vec::new());
    let mut module = TbModuleVtable::default();
    unsafe { initialize_ok(std::ptr::null(), &mut module) };
    transport.initialize(module).unwrap();
    REINITIALIZE_CODE.store(TB_TIMEOUT, Ordering::Release);
    let error = transport
        .reinitialize(crate::Secret::new(
            serde_json::to_vec(&serde_json::json!({ "secret": "never printed" })).unwrap(),
        ))
        .await
        .unwrap_err();
    REINITIALIZE_CODE.store(TB_OK, Ordering::Release);
    assert!(matches!(error, Error::Timeout { .. }), "{error}");
    assert!(!error.to_string().contains("never printed"));
}

#[tokio::test]
async fn delivery_maps_a_faulted_transport_a_missing_module_and_module_refusals() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    // No module installed yet.
    let (bare, _) = ModuleTransport::new("bare".to_string(), Vec::new());
    assert!(matches!(
        bare.deliver_now(call()).await,
        Err(Error::ConnectionClosed)
    ));

    // A module that refuses the frame, or fails in an unspecified way.
    let (transport, host) = ModuleTransport::new("refuses".to_string(), Vec::new());
    let mut module = TbModuleVtable::default();
    unsafe { initialize_ok(std::ptr::null(), &mut module) };
    transport.initialize(module).unwrap();
    DELIVERY_CODE.store(TB_BAD_ARGUMENT, Ordering::Release);
    let refused = transport.deliver_now(call()).await.unwrap_err();
    assert!(
        refused.to_string().contains("refused a valid frame"),
        "{refused}"
    );
    DELIVERY_CODE.store(77, Ordering::Release);
    let failed = transport.deliver_now(call()).await.unwrap_err();
    assert!(
        failed.to_string().contains("delivery callback failed"),
        "{failed}"
    );
    DELIVERY_CODE.store(TB_OK, Ordering::Release);

    // Once the module faults, delivery is refused before touching it.
    unsafe { (host.fault)(host.host_ctx, std::ptr::null(), 0) };
    assert!(matches!(
        transport.deliver_now(call()).await,
        Err(Error::ConnectionClosed)
    ));
    wait_physical_cleanup(&transport).await;
}

#[test]
fn native_provenance_requires_actual_broker_admission() {
    let (transport, host) = ModuleTransport::new("context".into(), b"{}".to_vec());
    let callback = host
        .broker_routing
        .expect("additive broker provenance callback");
    assert_eq!(unsafe { callback(host.host_ctx) }, 0);
    assert_eq!(unsafe { callback(std::ptr::null_mut()) }, 0);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    crate::broker::Broker::new().attach(transport);
    assert_eq!(unsafe { callback(host.host_ctx) }, 1);
}

unsafe extern "C" fn legacy_host_prefix_init(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    let prefix = unsafe {
        host.cast::<crate::module::abi::TbHostVtableV1Prefix>()
            .read()
    };
    assert!(prefix.size >= crate::module::abi::TB_HOST_VTABLE_BASE_SIZE);
    assert_eq!(prefix.config.len, 2);
    // An old module only copies these bytes and never interprets the tail.
    unsafe { initialize_ok(host, out) }
}

#[tokio::test]
async fn a_new_host_keeps_the_frozen_prefix_readable_by_old_modules() {
    let (transport, host) = ModuleTransport::new("legacy".into(), b"{}".to_vec());
    let mut module = TbModuleVtable::default();
    assert_eq!(
        unsafe { legacy_host_prefix_init(&host, &mut module) },
        TB_OK
    );
    transport.initialize(module).unwrap();
}

#[tokio::test]
async fn rejected_initialization_releases_observer_without_claiming_initializing() {
    let (transport, _) = ModuleTransport::new("rejected".into(), vec![]);
    transport.pause_initialization();
    let weak = Arc::downgrade(&transport);
    let observed = transport.clone();
    let observer = tokio::spawn(async move { observed.wait_initializing().await });
    tokio::task::yield_now().await;
    transport.release_initialization(false);
    assert!(transport.ensure_initialized().await.is_err());
    assert!(!transport.init_started());
    drop(transport);
    tokio::time::timeout(Duration::from_millis(100), observer)
        .await
        .unwrap()
        .unwrap();
    assert!(weak.upgrade().is_none());
}

#[tokio::test]
async fn stop_before_initialization_terminates_all_waiters() {
    let (transport, _) = ModuleTransport::new("stopped".into(), vec![]);
    transport.pause_initialization();
    transport.stop_sync(Duration::ZERO);
    tokio::time::timeout(Duration::from_millis(100), transport.ensure_initialized())
        .await
        .unwrap()
        .unwrap_err();
    tokio::time::timeout(Duration::from_millis(100), transport.wait_initializing())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_millis(100), transport.wait_ready())
        .await
        .unwrap();
    assert!(!transport.init_started());
    assert!(!transport.is_ready());
}

#[tokio::test]
async fn ready_followed_by_fault_is_not_successful_readiness() {
    let (transport, host) = ModuleTransport::new("ready-fault".into(), vec![]);
    unsafe {
        (host.ready)(host.host_ctx);
    }
    assert!(transport.is_ready());
    unsafe {
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
    }
    transport.wait_ready().await;
    assert!(transport.is_faulted());
    assert!(!transport.is_ready());
}

// Release opaque callbacks even when a regression assertion fails.
struct ShutdownRelease;
impl Drop for ShutdownRelease {
    fn drop(&mut self) {
        SHUTDOWN_BLOCKED.store(false, Ordering::Release);
        DELIVERY_BLOCKED.store(false, Ordering::Release);
        DELIVERY_CODE.store(TB_OK, Ordering::Release);
    }
}

async fn shutdown_entry() {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !SHUTDOWN_ENTERED.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S00");
}

#[tokio::test]
async fn ordinary_shutdown_deadline_preserves_state_until_callback_returns() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("S01");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_millis(100)));
    shutdown_entry().await;
    assert!(
        matches!(stopping.await.expect("S02"), Err(StopError::Started(_))),
        "S03"
    );
    assert!(!transport.is_faulted(), "S04");
    assert!(transport.is_ready(), "S05");
    assert!(
        matches!(
            transport.clone().stop(Duration::from_millis(10)).await,
            Err(StopError::NotStarted(_))
        ),
        "S06"
    );
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "S07");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let _completed = tokio::time::timeout(Duration::from_secs(2), transport.lifecycle.lock())
        .await
        .expect("S08");
    assert!(!transport.is_faulted(), "S09");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    assert!(!transport.is_ready(), "S10");
    assert!(
        matches!(transport.send(call()).await, Err(Error::ConnectionClosed)),
        "S11"
    );
    assert!(!transport.init_failed(), "S12");
    assert!(
        matches!(
            transport.clone().stop(Duration::from_secs(1)).await,
            Err(StopError::NotStarted(Error::ConnectionClosed))
        ),
        "S68"
    );
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "S69");
    assert!(
        matches!(
            transport
                .reinitialize(crate::Secret::new(b"{}".to_vec()))
                .await,
            Err(Error::ConnectionClosed)
        ),
        "S70"
    );
}

#[tokio::test]
async fn cancelling_a_started_shutdown_keeps_physical_exclusion_without_faulting() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("cancel-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("S13");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    shutdown_entry().await;
    stopping.abort();
    assert!(stopping.await.expect_err("S14").is_cancelled(), "S15");
    assert!(!transport.is_faulted(), "S16");
    assert!(
        matches!(
            transport.clone().stop(Duration::from_millis(10)).await,
            Err(StopError::NotStarted(_))
        ),
        "S17"
    );
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "S18");
    unsafe {
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
    }
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let _completed = tokio::time::timeout(Duration::from_secs(2), transport.lifecycle.lock())
        .await
        .expect("S19");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    assert!(transport.is_faulted(), "S20");
    assert!(!transport.is_ready(), "S21");
}

#[tokio::test]
async fn stopping_initialized_module_before_ready_releases_observers_and_queued_calls() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let (transport, host) = ModuleTransport::new("queued-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.expect("S22");
    assert!(transport.init_started(), "S23");
    transport.stop_sync(Duration::ZERO);
    tokio::time::timeout(Duration::from_secs(2), transport.wait_ready())
        .await
        .expect("S24");
    tokio::time::timeout(Duration::from_secs(2), async {
        while transport.drain_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S25");
    assert!(transport.pending.lock().expect("S36").is_empty(), "S37");
    assert!(!transport.is_faulted(), "S26");
    assert!(!transport.is_ready(), "S27");
    assert!(
        matches!(transport.send(call()).await, Err(Error::ConnectionClosed)),
        "S28"
    );
    assert!(!transport.init_failed(), "S29");
    let weak = Arc::downgrade(&transport);
    drop(transport);
    // drain_started and init publication precede the actual task's Arc drop.
    // Observe reclamation itself instead of treating either flag as task exit.
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S30");
}

#[tokio::test]
async fn ordinary_stop_wakes_backpressure_without_turning_closure_into_a_fault() {
    let _release = ShutdownRelease;
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    DELIVERY_CODE.store(TB_BACKPRESSURE, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("pressure-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.expect("S31");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while DELIVERIES.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S32");
    transport.stop_sync(Duration::ZERO);
    // An explicit legacy wake also must not cause delivery to re-enter disposed code.
    unsafe {
        (host.wake)(host.host_ctx);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while transport.drain_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S33");
    assert!(!transport.is_faulted(), "S34");
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 1, "S35");
    DELIVERY_CODE.store(TB_OK, Ordering::Release);
}

#[tokio::test]
async fn a_send_waiting_for_initialization_admission_closes_without_an_initializer_fault() {
    let (transport, host) = ModuleTransport::new("admission-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.pause_initialization();
    let sending_transport = transport.clone();
    let sending = tokio::spawn(async move { sending_transport.send(call()).await });
    tokio::task::yield_now().await;
    transport.stop_sync(Duration::ZERO);
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(2), sending)
                .await
                .expect("S38")
                .expect("S39"),
            Err(Error::ConnectionClosed)
        ),
        "S40"
    );
    assert!(!transport.init_started(), "S41");
    assert!(!transport.init_failed(), "S42");
    assert!(!transport.is_faulted(), "S43");
    assert!(transport.pending.lock().expect("S44").is_empty(), "S45");
    assert!(!transport.drain_started.load(Ordering::Acquire), "S46");
}

struct InitializerRelease(Option<std::sync::mpsc::SyncSender<()>>);
impl Drop for InitializerRelease {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn an_actual_initializer_failure_during_stop_is_retained() {
    let (transport, host) = ModuleTransport::new("failed-stop".into(), vec![]);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let release = InitializerRelease(Some(sender));
    let (entered, observed) = tokio::sync::oneshot::channel();
    transport.defer_initializer(host, move |_| {
        let _ = entered.send(());
        let _ = receiver.recv();
        Err("module initialization failed".into())
    });
    let sending_transport = transport.clone();
    let sending = tokio::spawn(async move { sending_transport.send(call()).await });
    tokio::time::timeout(Duration::from_secs(2), observed)
        .await
        .expect("S47")
        .expect("S48");
    transport.stop_sync(Duration::ZERO);
    drop(release);
    tokio::time::timeout(Duration::from_secs(2), sending)
        .await
        .expect("S49")
        .expect("S50")
        .expect("S51");
    assert!(transport.init_failed(), "S52");
    assert!(transport.is_faulted(), "S53");
    assert!(!transport.is_ready(), "S54");
}

#[tokio::test]
async fn concurrent_pending_sends_cannot_resurrect_a_completed_transport_drainer() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let (transport, host) = ModuleTransport::new("racing-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("S55");
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut sending = Vec::new();
    for _ in 0..16 {
        let sender = transport.clone();
        let barrier = barrier.clone();
        sending.push(tokio::spawn(async move {
            barrier.wait().await;
            sender.send(call()).await
        }));
    }
    barrier.wait().await;
    transport.stop_sync(Duration::ZERO);
    for sending in sending {
        assert!(
            matches!(
                sending.await.expect("S56"),
                Ok(()) | Err(Error::ConnectionClosed)
            ),
            "S57"
        );
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while transport.drain_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S58");
    assert!(transport.pending.lock().expect("S59").is_empty(), "S60");
    assert!(!transport.is_faulted(), "S61");
    assert!(!transport.init_failed(), "S62");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_actual_delivery_failure_overlapping_stop_is_retained() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    DELIVERY_CODE.store(TB_BAD_ARGUMENT, Ordering::Release);
    DELIVERY_BLOCKED.store(true, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("delivery-failed-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.expect("S63");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while DELIVERIES.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S64");
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !transport.admission_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A57");
    assert!(!SHUTDOWN_ENTERED.load(Ordering::Acquire), "A58");
    DELIVERY_BLOCKED.store(false, Ordering::Release);
    stopping.await.expect("A59").expect("A60");
    tokio::time::timeout(Duration::from_secs(2), async {
        while transport.drain_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("S65");
    assert!(transport.is_faulted(), "S66");
    assert!(!transport.is_ready(), "S67");
}

#[tokio::test]
async fn lifecycle_waiters_do_not_reenter_a_transport_disposed_by_the_prior_shutdown() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("lifecycle-stop".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("S71");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_millis(100)));
    shutdown_entry().await;
    assert!(
        matches!(stopping.await.expect("S72"), Err(StopError::Started(_))),
        "S73"
    );
    let mut retry = std::pin::pin!(transport.clone().stop(Duration::from_secs(2)));
    let mut reinitializing =
        std::pin::pin!(transport.reinitialize(crate::Secret::new(b"{}".to_vec())));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        std::future::Future::poll(retry.as_mut(), &mut context).is_pending(),
        "S74"
    );
    assert!(
        matches!(
            std::future::Future::poll(reinitializing.as_mut(), &mut context),
            std::task::Poll::Ready(Err(Error::ConnectionClosed))
        ),
        "S75"
    );
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    assert!(
        matches!(
            retry.await,
            Err(StopError::NotStarted(Error::ConnectionClosed))
        ),
        "S76"
    );

    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "S78");
    assert!(transport.stop_task_spawned(), "S79");
    assert!(!transport.is_faulted(), "S80");
}

#[tokio::test]
async fn fresh_signal_and_call_cannot_enter_an_actual_running_shutdown() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("admission".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("A01");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    shutdown_entry().await;
    let mut signal = call();
    signal.header.kind = MessageKind::Signal;
    let signal_result = transport.send(signal).await;
    let call_result = transport.send(call()).await;
    assert!(matches!(signal_result, Err(Error::ConnectionClosed)), "A02");
    assert!(matches!(call_result, Err(Error::ConnectionClosed)), "A03");
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 0, "A04");
    assert!(!transport.is_faulted(), "A05");
    assert!(!transport.init_failed(), "A06");
    assert!(!transport.is_stopped(), "A07");
    assert!(transport.is_ready(), "A08");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    assert_eq!(stopping.await.expect("A09").expect("A10"), TB_OK, "A11");
}

#[tokio::test]
async fn queued_readiness_and_backpressure_retire_before_actual_shutdown_returns() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("queued-admission".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.expect("A12");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    shutdown_entry().await;
    tokio::time::timeout(Duration::from_millis(100), transport.wait_ready())
        .await
        .expect("A13");
    unsafe {
        (host.ready)(host.host_ctx);
        (host.wake)(host.host_ctx);
    }
    tokio::time::timeout(Duration::from_millis(100), async {
        while transport.drain_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A14");
    let empty = transport.pending.lock().expect("A15").is_empty();
    assert!(empty, "A16");
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 0, "A17");
    assert!(!transport.is_ready(), "A18");
    assert!(!transport.is_faulted(), "A19");
    assert!(!transport.is_stopped(), "A20");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    stopping.await.expect("A21").expect("A22");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_signal_callback_is_drained_despite_stop_timeout() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    DELIVERY_BLOCKED.store(true, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("callback-drain".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("A23");
    let mut signal = call();
    signal.header.kind = MessageKind::Signal;
    let sending_transport = transport.clone();
    let sending = tokio::spawn(async move { sending_transport.send(signal).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while DELIVERIES.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A24");
    assert_eq!(transport.inflight(), 0, "A25");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_millis(50)));
    assert!(
        matches!(stopping.await.expect("A26"), Err(StopError::Started(_))),
        "A27"
    );
    assert!(!SHUTDOWN_ENTERED.load(Ordering::Acquire), "A28");
    assert!(!transport.is_stopped(), "A29");
    assert!(transport.stop_task_spawned(), "A30");
    assert!(
        matches!(
            transport.clone().stop(Duration::from_millis(10)).await,
            Err(StopError::NotStarted(_))
        ),
        "A31"
    );
    assert!(
        matches!(transport.send(call()).await, Err(Error::ConnectionClosed)),
        "A32"
    );
    DELIVERY_BLOCKED.store(false, Ordering::Release);
    sending.await.expect("A33").expect("A34");
    shutdown_entry().await;
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "A35");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let _completed = tokio::time::timeout(Duration::from_secs(2), transport.lifecycle.lock())
        .await
        .expect("A36");
    assert!(transport.is_stopped(), "A37");
    assert!(!transport.is_faulted(), "A38");
}

#[tokio::test]
async fn installed_vtable_without_init_flag_closes_fresh_admission_before_teardown() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("installed-admission".into(), vec![]);
    let mut module = TbModuleVtable::default();
    unsafe {
        initialize_ok(&host, &mut module);
    }
    transport.initialize(module).expect("A39");
    assert!(!transport.init_started(), "A40");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    shutdown_entry().await;
    let result = transport.send(call()).await;
    assert!(matches!(result, Err(Error::ConnectionClosed)), "A41");
    assert!(!transport.init_started(), "A42");
    assert!(!transport.is_stopped(), "A43");
    assert!(!transport.is_faulted(), "A44");
    assert!(!transport.init_failed(), "A45");
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 0, "A46");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    stopping.await.expect("A47").expect("A48");
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "A49");
}

#[tokio::test]
async fn backpressure_retry_closes_without_fault_while_actual_shutdown_is_blocked() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    DELIVERY_CODE.store(TB_BACKPRESSURE, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("pressure-admission".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.send(call()).await.expect("A50");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while DELIVERIES.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A51");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    shutdown_entry().await;
    tokio::time::timeout(Duration::from_millis(100), async {
        while transport.drain_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A52");
    unsafe {
        (host.wake)(host.host_ctx);
    }
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 1, "A53");
    assert!(!transport.is_faulted(), "A54");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    stopping.await.expect("A55").expect("A56");
}

#[tokio::test]
async fn a_reserved_native_entry_cannot_start_after_teardown_even_if_stop_caller_is_cancelled() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(true, Ordering::Release);
    SHUTDOWN_ENTERED.store(false, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    DELIVERIES.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("reserved-admission".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("A61");
    // Pause at the real check-to-native-entry boundary: the reservation already
    // belongs to the callback caller, but native entry has not happened yet.
    let lease = transport.callbacks.enter().expect("A62");
    let stopping = tokio::spawn(transport.clone().stop(Duration::from_secs(2)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !transport.admission_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A63");
    stopping.abort();
    assert!(stopping.await.expect_err("A64").is_cancelled(), "A65");
    assert!(!SHUTDOWN_ENTERED.load(Ordering::Acquire), "A66");
    assert!(!transport.is_stopped(), "A67");
    assert!(transport.callbacks.enter().is_err(), "A68");
    assert!(
        matches!(
            transport.ensure_initialized().await,
            Err(Error::ConnectionClosed)
        ),
        "A69"
    );
    assert!(
        matches!(
            transport.reinitialize(crate::Secret::new(vec![])).await,
            Err(Error::ConnectionClosed)
        ),
        "A70"
    );
    assert!(
        matches!(
            transport.clone().stop(Duration::from_millis(10)).await,
            Err(StopError::NotStarted(_))
        ),
        "A71"
    );
    // An admitted callback can still enter before teardown; shutdown must wait
    // for its physical exit, even after the outer stop future disappears.
    let module = *transport.module.lock().expect("A72");
    let module = module.expect("A73");
    let bytes = serde_json::to_vec(&call()).expect("A74");
    assert_eq!(
        unsafe { (module.deliver)(module.module_ctx, bytes.as_ptr(), bytes.len()) },
        TB_OK,
        "A75"
    );
    assert!(!SHUTDOWN_ENTERED.load(Ordering::Acquire), "A76");
    drop(lease);
    shutdown_entry().await;
    assert_eq!(DELIVERIES.load(Ordering::Acquire), 1, "A77");
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1, "A78");
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let _completed = tokio::time::timeout(Duration::from_secs(2), transport.lifecycle.lock())
        .await
        .expect("A79");
    assert!(transport.is_stopped(), "A80");
    assert!(!transport.is_faulted(), "A81");
}

#[tokio::test]
async fn unanswered_method_calls_are_not_physical_callback_leases() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let _release = ShutdownRelease;
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let (transport, host) = ModuleTransport::new("unanswered".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.expect("A82");
    unsafe {
        (host.ready)(host.host_ctx);
    }
    transport.send(call()).await.expect("A83");
    assert_eq!(transport.inflight(), 1, "A84");
    assert_eq!(
        transport
            .clone()
            .stop(Duration::from_secs(2))
            .await
            .expect("A85"),
        TB_OK,
        "A86"
    );
    assert_eq!(transport.inflight(), 1, "A87");
    assert!(transport.is_stopped(), "A88");
}

#[tokio::test]
async fn abandoning_initiating_waiter_faults_and_retains_late_initializer_cleanup() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    SHUTDOWN_CODE.store(TB_OK, Ordering::Release);
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    let (transport, host) = ModuleTransport::new("abandoned-init".into(), b"secret".to_vec());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    transport.defer_initializer(host, move |host| {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        assert_eq!(
            unsafe { std::slice::from_raw_parts(host.config.ptr, host.config.len) },
            b"secret"
        );
        let mut module = TbModuleVtable::default();
        assert_eq!(unsafe { initialize_ok(&host, &mut module) }, TB_OK);
        Ok(module)
    });
    let waiter = tokio::spawn({
        let transport = transport.clone();
        async move { transport.ensure_initialized().await }
    });
    entered_rx.await.unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    // Always release foreign work even on the unfixed implementation.
    let faulted = transport.is_faulted();
    release_tx.send(()).unwrap();
    assert!(
        faulted,
        "initiating waiter abandonment must publish fault immediately"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if transport.is_stopped() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 1);
    assert!(transport.ensure_initialized().await.is_err());
}

#[tokio::test]
async fn scheduled_initializer_fault_cancels_before_actual_entry() {
    let (transport, host) = ModuleTransport::new("scheduled-fault".into(), vec![]);
    let entries = Arc::new(AtomicUsize::new(0));
    transport.defer_initializer(host, {
        let entries = entries.clone();
        move |_| {
            entries.fetch_add(1, Ordering::AcqRel);
            Err("unexpected C entry".into())
        }
    });
    let guard = transport.lifecycle.clone().lock_owned().await;
    assert!(transport.schedule_initializer().unwrap());
    transport.request_terminal(true);
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    assert_eq!(
        transport.context.physical.lock().unwrap().init,
        InitState::Scheduled
    );
    drop(guard);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !transport.is_stopped() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(entries.load(Ordering::Acquire), 0);
    assert_eq!(
        transport.context.physical.lock().unwrap().cleanup,
        CleanupState::NoCallbackRequired
    );
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
}

#[tokio::test]
async fn scheduled_initializer_is_adopted_by_original_ordinary_stop() {
    let (transport, host) = ModuleTransport::new("scheduled-stop".into(), vec![]);
    let entries = Arc::new(AtomicUsize::new(0));
    transport.defer_initializer(host, {
        let entries = entries.clone();
        move |_| {
            entries.fetch_add(1, Ordering::AcqRel);
            Err("unexpected C entry".into())
        }
    });
    let (worker_started, worker_started_rx) = tokio::sync::oneshot::channel();
    let (worker_release, worker_release_rx) = std::sync::mpsc::channel();
    *transport.before_worker_lock.lock().unwrap() = Some(Box::new(move || {
        worker_started.send(()).unwrap();
        worker_release_rx.recv().unwrap();
    }));
    let waiter = tokio::spawn({
        let transport = transport.clone();
        async move { transport.ensure_initialized().await }
    });
    worker_started_rx.await.unwrap();
    let mut stopping = Box::pin(transport.clone().stop(Duration::from_secs(2)));
    tokio::select! { result = &mut stopping => panic!("stop completed before owner: {result:?}"),
    _ = async { while !transport.stop_task_spawned() { tokio::task::yield_now().await; } } => {} }
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(
        unsafe { host_native_terminal_state(host.host_ctx) },
        2,
        "ordinary cancellation cannot invent Fault"
    );
    worker_release.send(()).unwrap();
    assert_eq!(stopping.await.unwrap(), TB_CLOSED);
    assert_eq!(entries.load(Ordering::Acquire), 0);
    assert!(!transport.is_faulted());
}

#[tokio::test]
async fn rejected_spawn_retains_actual_initializer_owner_and_never_resubmits() {
    struct Probe(Arc<AtomicBool>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let (transport, host) = ModuleTransport::new("spawn-rejection".into(), vec![]);
    transport.reject_worker_spawn.store(true, Ordering::Release);
    let probe = Probe(dropped.clone());
    transport.defer_initializer(host, move |_| {
        drop(probe);
        Err("must never execute".into())
    });
    assert!(transport.ensure_initialized().await.is_err());
    assert!(transport.is_faulted());
    let weak = Arc::downgrade(&transport);
    assert_eq!(
        transport.context.physical.lock().unwrap().cleanup,
        CleanupState::Uncertain
    );
    transport
        .reject_worker_spawn
        .store(false, Ordering::Release);
    assert!(transport.ensure_initialized().await.is_err());
    drop(transport);
    assert!(
        weak.upgrade().is_some(),
        "context anchor retains actual job through host teardown"
    );
    assert!(
        !dropped.load(Ordering::Acquire),
        "closure rejection must not drop controller"
    );
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeControls {
    ctx: *mut c_void,
    entered: unsafe extern "C" fn(*mut c_void, *const TbHostVtable),
    joined: unsafe extern "C" fn(*mut c_void),
}
pub(crate) struct NativeBarrier {
    pub(crate) state: StdMutex<(Option<TbHostVtable>, bool, bool)>,
    pub(crate) changed: Condvar,
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) joined: tokio::sync::Notify,
    pub(crate) finish_gate: StdMutex<Option<std::sync::mpsc::Receiver<()>>>,
}
unsafe impl Send for NativeBarrier {}
unsafe impl Sync for NativeBarrier {}
unsafe extern "C" fn native_entered(ctx: *mut c_void, host: *const TbHostVtable) {
    let barrier = unsafe { &*ctx.cast::<NativeBarrier>() };
    let mut state = barrier
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.0 = Some(unsafe { *host });
    barrier.entered.notify_one();
    while !state.1 {
        state = barrier
            .changed
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}
unsafe extern "C" fn native_joined(ctx: *mut c_void) {
    let barrier = unsafe { &*ctx.cast::<NativeBarrier>() };
    let mut state = barrier.state.lock().unwrap();
    assert!(!state.2, "one actual native shutdown/join");
    state.2 = true;
    barrier.joined.notify_one();
    drop(state);
    let gate = barrier
        .finish_gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    // This artificial gate runs after the fixture's actual worker join. A failed
    // Rust assertion may disconnect it; never unwind through the C callback.
    if let Some(gate) = gate {
        let _ = gate.recv();
    }
}
pub(crate) fn native_fixture() -> (
    Arc<ModuleTransport>,
    TbHostVtable,
    crate::module::abi::TbModuleInit,
    Arc<NativeBarrier>,
) {
    let path = std::env::var_os("TINYBUS_TEST_CONTROLLED_INIT_MODULE")
        .expect("controlled native fixture path");
    let artifact = crate::module::loader::load(std::path::Path::new(&path), false).unwrap();
    let barrier = Arc::new(NativeBarrier {
        state: StdMutex::new((None, false, false)),
        changed: Condvar::new(),
        entered: Notify::new(),
        joined: Notify::new(),
        finish_gate: StdMutex::new(None),
    });
    let controls = NativeControls {
        ctx: Arc::as_ptr(&barrier).cast_mut().cast(),
        entered: native_entered,
        joined: native_joined,
    };
    let config = unsafe {
        std::slice::from_raw_parts(
            std::ptr::from_ref(&controls).cast::<u8>(),
            size_of::<NativeControls>(),
        )
    }
    .to_vec();
    let (transport, host) = ModuleTransport::new("actual-native".into(), config);
    *transport.fixture_controller.lock().unwrap() = Some(barrier.clone());
    (transport, host, artifact.init, barrier)
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_controller_is_owned_until_shutdown_callback_returns() {
    let (transport, host, init, barrier) = native_fixture();
    barrier.state.lock().unwrap().1 = true;
    transport.defer_initialize(init, host);
    transport.ensure_initialized().await.unwrap();
    barrier.entered.notified().await;
    let weak = Arc::downgrade(&barrier);
    let (release, gate) = std::sync::mpsc::channel();
    *barrier.finish_gate.lock().unwrap() = Some(gate);
    let stopping = tokio::spawn({
        let transport = transport.clone();
        async move { transport.stop(Duration::from_secs(2)).await }
    });
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    // Observe ownership safely while the real native callback still borrows
    // the controller. Always release C before a regression assertion can panic.
    let retained = Arc::strong_count(&barrier) > 1;
    let cleanup_incomplete =
        transport.context.physical.lock().unwrap().cleanup != CleanupState::Complete;
    release.send(()).unwrap();
    assert_eq!(stopping.await.unwrap().unwrap(), TB_OK);
    assert!(cleanup_incomplete);
    drop(barrier);
    let retained_through_transport = weak.upgrade().is_some();
    drop(transport);
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successful fixture owner is reclaimed after actual callback return");
    assert!(
        retained,
        "physical owner must retain the borrowed native controller"
    );
    assert!(retained_through_transport);
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_late_init_is_joined_after_initiator_abandonment() {
    let (transport, host, init, barrier) = native_fixture();
    transport.defer_initialize(init, host);
    let waiter = tokio::spawn({
        let transport = transport.clone();
        async move { transport.ensure_initialized().await }
    });
    barrier.entered.notified().await;
    let mut observation = Box::pin(transport.ensure_initialized());
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(observation.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(observation);
    assert_eq!(
        unsafe { host_native_terminal_state(host.host_ctx) },
        1,
        "observer abandonment cannot cancel initiating owner"
    );
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let view = barrier.state.lock().unwrap().0.unwrap();
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        6
    );
    assert!(!barrier.state.lock().unwrap().2);
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    let _physical_exit = transport.lifecycle.lock().await;
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        14
    );
    assert_eq!(
        transport.context.physical.lock().unwrap().cleanup,
        CleanupState::Complete
    );
    assert!(transport.ensure_initialized().await.is_err());
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_eager_fault_denies_while_config_remains_borrowed_then_joins() {
    let (transport, host, init, barrier) = native_fixture();
    let host = DeferredInitializer {
        initialize: Box::new(|_| unreachable!()),
        host,
    };
    let eager = std::thread::spawn({
        let transport = transport.clone();
        move || {
            let owner = host;
            transport.eager_initialize(init, owner.host)
        }
    });
    barrier.entered.notified().await;
    let view = barrier.state.lock().unwrap().0.unwrap();
    unsafe {
        (view.fault)(view.host_ctx, std::ptr::null(), 0);
    }
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        6
    );
    assert_eq!(
        transport.context.config.lock().unwrap().len(),
        size_of::<NativeControls>()
    );
    assert!(!barrier.state.lock().unwrap().2);
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    assert!(eager.join().unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    let _physical_exit = transport.lifecycle.lock().await;
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        14
    );
    assert!(transport.context.config.lock().unwrap().is_empty());
}

async fn wait_physical_cleanup(transport: &ModuleTransport) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let changed = transport.context.physical_notify.notified();
            let terminal = matches!(
                transport.context.physical.lock().unwrap().cleanup,
                CleanupState::Complete | CleanupState::Uncertain | CleanupState::NoCallbackRequired
            );
            if terminal {
                return;
            }
            changed.await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn observing_fault_owned_job_does_not_create_ordinary_stop_marker() {
    let (transport, host) = ModuleTransport::new("fault-owner".into(), vec![]);
    transport.reject_worker_spawn.store(true, Ordering::Release);
    transport.defer_initialize(initialize_ok, host);
    assert!(transport.ensure_initialized().await.is_err());
    assert!(!transport.stop_task_spawned());
    assert!(matches!(
        transport.clone().stop(Duration::from_secs(1)).await,
        Err(StopError::NotStarted(_))
    ));
    assert!(
        !transport.stop_task_spawned(),
        "fault-owned uncertainty cannot become an ordinary handoff"
    );
}

#[tokio::test]
async fn ordinary_shutdown_retains_non_ok_return_without_physical_success() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let (transport, host) = ModuleTransport::new("shutdown-status".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.unwrap();
    SHUTDOWN_CODE.store(TB_TIMEOUT, Ordering::Release);
    SHUTDOWN_BLOCKED.store(false, Ordering::Release);
    let result = transport.clone().stop(Duration::from_secs(1)).await;
    SHUTDOWN_CODE.store(TB_OK, Ordering::Release);
    assert_eq!(
        result.unwrap(),
        TB_TIMEOUT,
        "ordinary public return remains compatible"
    );
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 2);
    assert_eq!(
        transport.context.physical.lock().unwrap().cleanup,
        CleanupState::Uncertain
    );
    assert!(transport.stop_task_spawned());
    assert!(
        transport.is_stopped(),
        "legacy stopped is separate from physical-success proof"
    );
    assert!(!transport.is_faulted());
}

#[tokio::test]
async fn rejected_module_outputs_never_invoke_untrusted_callbacks() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    for rejection in 0..4 {
        let (transport, host) = ModuleTransport::new(format!("rejected-{rejection}"), vec![]);
        let init: crate::module::abi::TbModuleInit = match rejection {
            0 => initialize_short_output,
            1 => initialize_null_context_output,
            2 => initialize_missing_declared_reinit,
            _ => initialize_non_ok_misleading_output,
        };
        transport.defer_initialize(init, host);
        assert!(transport.ensure_initialized().await.is_err());
        let physical = transport.context.physical.lock().unwrap();
        assert_eq!(physical.init, InitState::PublishedRejected);
        assert_eq!(physical.cleanup, CleanupState::Uncertain);
        assert!(physical.job.is_some());
        assert!(transport.module.lock().unwrap().is_none());
        drop(physical);
        assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    }
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn rejected_ordinary_worker_submission_retains_original_lifecycle_guard() {
    let (transport, host) = ModuleTransport::new("stop-spawn-rejection".into(), vec![]);
    transport.defer_initialize(initialize_ok, host);
    transport.ensure_initialized().await.unwrap();
    transport.reject_worker_spawn.store(true, Ordering::Release);
    assert!(matches!(
        transport.clone().stop(Duration::from_secs(1)).await,
        Err(StopError::Started(_))
    ));
    assert!(transport.stop_task_spawned());
    assert!(
        transport.lifecycle.try_lock().is_err(),
        "rejected closure cannot drop actual acquired guard"
    );
    let weak = Arc::downgrade(&transport);
    drop(transport);
    let retained = weak
        .upgrade()
        .expect("actual owner retained beyond host handle");
    assert_eq!(
        retained.context.physical.lock().unwrap().cleanup,
        CleanupState::Uncertain
    );
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 2);
    assert!(
        !retained.is_faulted(),
        "ordinary spawner failure is uncertainty, not module fault"
    );
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_deadline_fault_retains_late_cleanup_without_wall_clock_sleep() {
    let (transport, host, init, barrier) = native_fixture();
    transport.defer_initialize(init, host);
    let waiter = tokio::spawn({
        let transport = transport.clone();
        async move { transport.ensure_initialized().await }
    });
    barrier.entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(MODULE_INIT_DEADLINE).await;
    assert!(waiter.await.unwrap().is_err());
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    assert!(!barrier.state.lock().unwrap().2);
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    // Resume the real timer only for the outer stuck-test safety deadline.
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    wait_physical_cleanup(&transport).await;
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 14);
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_fault_between_return_and_publication_uses_same_owner() {
    let (transport, host, init, barrier) = native_fixture();
    let (publication_tx, publication_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *transport.before_init_publication.lock().unwrap() = Some(Box::new(move || {
        publication_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    }));
    transport.defer_initialize(init, host);
    let waiter = tokio::spawn({
        let transport = transport.clone();
        async move { transport.ensure_initialized().await }
    });
    barrier.entered.notified().await;
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    publication_rx.await.unwrap();
    let job = transport
        .context
        .physical
        .lock()
        .unwrap()
        .job
        .clone()
        .unwrap();
    unsafe {
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
    }
    {
        let state = transport.context.physical.lock().unwrap();
        assert!(Arc::ptr_eq(&job, state.job.as_ref().unwrap()));
        assert_eq!(state.cleanup, CleanupState::Requested);
        assert!(
            state.result.is_none(),
            "cleanup must not claim an empty publication"
        );
    }
    assert!(!barrier.state.lock().unwrap().2);
    release_tx.send(()).unwrap();
    assert!(waiter.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    wait_physical_cleanup(&transport).await;
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 14);
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_reinitializer_fault_denies_before_c_returns_and_joins_once() {
    let (transport, host, init, barrier) = native_fixture();
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
    }
    transport.defer_initialize(init, host);
    transport.ensure_initialized().await.unwrap();
    barrier.entered.notified().await;
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = false;
    }
    let reinit = tokio::spawn({
        let transport = transport.clone();
        async move { transport.reinitialize(crate::Secret::new(vec![])).await }
    });
    barrier.entered.notified().await;
    unsafe {
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
    }
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    assert!(!barrier.state.lock().unwrap().2);
    assert!(
        transport.lifecycle.try_lock().is_err(),
        "actual C reinit owns physical exclusion"
    );
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    reinit.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    wait_physical_cleanup(&transport).await;
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 14);
    assert!(
        transport
            .reinitialize(crate::Secret::new(vec![]))
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_late_eager_publication_cannot_refund_failed_cleanup_spawn() {
    let (transport, host, init, barrier) = native_fixture();
    transport.reject_worker_spawn.store(true, Ordering::Release);
    let host_owner = DeferredInitializer {
        initialize: Box::new(|_| unreachable!()),
        host,
    };
    let eager = std::thread::spawn({
        let transport = transport.clone();
        move || {
            let owner = host_owner;
            transport.eager_initialize(init, owner.host)
        }
    });
    barrier.entered.notified().await;
    unsafe {
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
    }
    let job = transport
        .context
        .physical
        .lock()
        .unwrap()
        .job
        .clone()
        .unwrap();
    assert_eq!(
        transport.context.physical.lock().unwrap().cleanup,
        CleanupState::Uncertain
    );
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    assert!(eager.join().unwrap().is_err());
    transport
        .reject_worker_spawn
        .store(false, Ordering::Release);
    transport.request_terminal(true);
    {
        let state = transport.context.physical.lock().unwrap();
        assert_eq!(state.init, InitState::ExternalValid);
        assert_eq!(state.cleanup, CleanupState::Uncertain);
        assert!(Arc::ptr_eq(&job, state.job.as_ref().unwrap()));
    }
    assert!(
        transport.module.lock().unwrap().is_some(),
        "valid late output is cleanup access only"
    );
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    assert!(
        !barrier.state.lock().unwrap().2,
        "rejected submission must never be silently retried"
    );
    let weak = Arc::downgrade(&transport);
    drop(transport);
    drop(job);
    assert!(weak.upgrade().is_some());
    // The existing uncertain physical owner retains this one controller too.
    // No separate leaked controller is needed; this is not global leak-bound proof.
    let controller = Arc::downgrade(&barrier);
    drop(barrier);
    assert!(controller.upgrade().is_some());
}

unsafe extern "C" fn initialize_short_output(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    unsafe {
        initialize_ok(host, out);
        (*out).size = TB_MODULE_VTABLE_BASE_SIZE - 1;
    }
    TB_OK
}
unsafe extern "C" fn initialize_null_context_output(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    unsafe {
        initialize_ok(host, out);
        (*out).module_ctx = std::ptr::null_mut();
    }
    TB_OK
}
unsafe extern "C" fn initialize_missing_declared_reinit(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    unsafe {
        initialize_ok(host, out);
        (*out).reinitialize = None;
    }
    TB_OK
}
unsafe extern "C" fn initialize_non_ok_misleading_output(
    host: *const TbHostVtable,
    out: *mut TbModuleVtable,
) -> i32 {
    unsafe {
        initialize_ok(host, out);
    }
    TB_PANICKED
}

#[tokio::test]
async fn eager_rejected_outputs_keep_uncertainty_and_never_call_returned_callbacks() {
    let _lock = VTABLE_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    SHUTDOWN_CALLS.store(0, Ordering::Release);
    for init in [
        initialize_short_output as crate::module::abi::TbModuleInit,
        initialize_null_context_output,
        initialize_missing_declared_reinit,
        initialize_non_ok_misleading_output,
    ] {
        let (transport, host) = ModuleTransport::new("eager-invalid".into(), b"borrowed".to_vec());
        assert!(transport.eager_initialize(init, host).is_err());
        let state = transport.context.physical.lock().unwrap();
        assert_eq!(state.init, InitState::PublishedRejected);
        assert_eq!(state.cleanup, CleanupState::Uncertain);
        assert!(state.job.is_some());
        assert!(transport.module.lock().unwrap().is_none());
        drop(state);
        assert!(transport.context.config.lock().unwrap().is_empty());
        assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    }
    assert_eq!(SHUTDOWN_CALLS.load(Ordering::Acquire), 0);
}

#[test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
fn actual_native_initializer_owner_outlives_runtime_and_host_handle_drop() {
    let (transport, host, init, barrier) = native_fixture();
    transport.defer_initialize(init, host);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let transport = transport.clone();
        tokio::spawn(async move {
            let _ = transport.ensure_initialized().await;
        });
        barrier.entered.notified().await;
    });
    let weak = Arc::downgrade(&transport);
    drop(runtime);
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 6);
    drop(transport);
    let retained = weak
        .upgrade()
        .expect("real initializer owner survives runtime and host drop");
    assert!(!barrier.state.lock().unwrap().2);
    {
        let mut state = barrier.state.lock().unwrap();
        state.1 = true;
        barrier.changed.notify_all();
    }
    // The runtime is gone. Wait on the retained physical predicate/Condvar,
    // never on a Tokio task, deadline or notification as completion proof.
    let mut state = retained.context.physical.lock().unwrap();
    while state.cleanup != CleanupState::Complete {
        let (next, timeout) = retained
            .context
            .physical_changed
            .wait_timeout(state, Duration::from_secs(2))
            .unwrap();
        state = next;
        assert!(
            !timeout.timed_out(),
            "actual retained native cleanup must finish"
        );
    }
    drop(state);
    assert!(barrier.state.lock().unwrap().2);
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 14);
    drop(retained);
    assert_eq!(unsafe { host_native_terminal_state(host.host_ctx) }, 14);
}

pub(crate) fn native_fixture_json(transport: &ModuleTransport) -> serde_json::Value {
    serde_json::json!({"test_controls": *transport.context.config.lock().unwrap()})
}

#[tokio::test]
async fn rejected_observer_after_ordinary_stop_preserves_nonfault_cause() {
    let (transport, view) = ModuleTransport::new("ordinary-cancellation".into(), vec![]);
    let _ = transport.clone().stop(Duration::from_secs(1)).await;
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        2
    );
    transport.release_initialization(false);
    assert!(!transport.is_faulted());
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        2
    );
}
