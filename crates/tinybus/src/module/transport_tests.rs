use super::*;
use std::sync::atomic::AtomicI32;

use crate::{BusName, InterfaceName, MemberName, ObjectPath};

static DELIVERY_CODE: AtomicI32 = AtomicI32::new(TB_OK);
static DELIVERIES: AtomicUsize = AtomicUsize::new(0);
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
        .reinitialize(serde_json::json!({ "secret": "never printed" }))
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
            .reinitialize(serde_json::json!({}))
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
            .reinitialize(serde_json::json!({ "secret": "never printed" }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!error.to_string().contains("never printed"));
    }
    REINITIALIZE_CODE.store(TB_OK, Ordering::Release);
    transport
        .reinitialize(serde_json::json!({ "replacement": true }))
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
            .reinitialize(serde_json::json!({ "generation": 1 }))
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
            transport.reinitialize(serde_json::json!({ "generation": 2 })),
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
