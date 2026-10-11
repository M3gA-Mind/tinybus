use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tinybus::{broker::Broker, module::ModuleHost, transport::memory::MemoryBus};

type SetupFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
type SetupFn = fn(Connection, Config) -> SetupFuture;
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static FACTORIES: AtomicUsize = AtomicUsize::new(0);
#[derive(serde::Deserialize)]
struct Config {
    restricted: bool,
}
struct Service {
    restricted: bool,
}
#[async_trait::async_trait]
impl tinybus::service::Interface for Service {
    fn name(&self) -> tinybus::InterfaceName {
        "ai.tinyhumans.tinybus.Factory".try_into().unwrap()
    }
    fn members(&self) -> Vec<tinybus::MemberName> {
        vec!["Mode".try_into().unwrap()]
    }
    async fn call(
        &self,
        _member: &tinybus::MemberName,
        _args: serde_json::Value,
    ) -> tinybus::Result<serde_json::Value> {
        Ok(serde_json::json!(self.restricted))
    }
}
async fn legacy_setup(connection: Connection, config: Config) -> Result<()> {
    connection
        .serve_at(
            "/factory".try_into()?,
            Service {
                restricted: config.restricted,
            },
        )
        .await?;
    connection
        .request_name("ai.tinyhumans.tinybus.Factory")
        .await?;
    Ok(())
}
fn setup_factory()
-> impl Fn(Connection, Config) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Clone + Send + Sync
{
    FACTORIES.fetch_add(1, Ordering::SeqCst);
    let mode = Arc::new(tokio::sync::Mutex::new(None));
    move |connection, config: Config| {
        let mode = Arc::clone(&mode);
        Box::pin(async move {
            let mut admitted = mode.lock().await;
            if admitted.is_some_and(|previous| previous != config.restricted) {
                return Err(tinybus::Error::failed("immutable mode"));
            }
            *admitted = Some(config.restricted);
            legacy_setup(connection, config).await
        })
    }
}
module_export_static! {
    setup_factory = setup_factory,
    config = Config,
    worker_threads = 1,
    provides = ["ai.tinyhumans.tinybus.Factory"],
    methods = ["Mode"], signals = [], requires = [], optional = [], lazy = false,
}
async fn instance(restricted: bool) -> (ModuleHost, Connection, tinybus::Proxy) {
    let bus = MemoryBus::new();
    let broker = Broker::new();
    broker.clone().spawn(bus.clone());
    let host = ModuleHost::new(broker);
    host.attach_linked_with_config(
        linked_module().unwrap(),
        serde_json::json!({"restricted":restricted}),
    )
    .unwrap();
    let caller = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let proxy = caller
        .proxy(
            "ai.tinyhumans.tinybus.Factory",
            "/factory",
            "ai.tinyhumans.tinybus.Factory",
        )
        .unwrap();
    assert_eq!(proxy.call::<bool>("Mode", ()).await.unwrap(), restricted);
    (host, caller, proxy)
}
#[tokio::test(flavor = "current_thread")]
async fn compiled_factory_is_per_instance_and_reinitialization_keeps_its_admission() {
    let _guard = TEST_LOCK.lock().await;
    let before = FACTORIES.load(Ordering::SeqCst);
    let ((first, _first_caller, first_proxy), (second, _second_caller, second_proxy)) =
        tokio::join!(instance(true), instance(false));
    assert_eq!(
        FACTORIES.load(Ordering::SeqCst) - before,
        2,
        "factory must run once per ABI init"
    );
    let (same, changed) = tokio::join!(
        first.reinitialize("tinybus-module", serde_json::json!({"restricted":true})),
        second.reinitialize("tinybus-module", serde_json::json!({"restricted":true})),
    );
    assert!(same.is_ok());
    assert!(changed.is_err());
    assert!(first_proxy.call::<bool>("Mode", ()).await.unwrap());
    assert!(!second_proxy.call::<bool>("Mode", ()).await.unwrap());
    assert_eq!(
        FACTORIES.load(Ordering::SeqCst) - before,
        2,
        "reinitialization must reuse factory state"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn compiled_instance_refuses_mode_change_without_replacing_live_service() {
    let _guard = TEST_LOCK.lock().await;
    let (host, _caller, proxy) = instance(true).await;
    let result = host
        .reinitialize("tinybus-module", serde_json::json!({"restricted":false}))
        .await;
    assert!(
        result.is_err(),
        "setup factory admission must survive reinitialization"
    );
    assert!(proxy.call::<bool>("Mode", ()).await.unwrap());
}
#[tokio::test]
async fn captured_admission_serializes_concurrent_setup_attempts() {
    let _guard = TEST_LOCK.lock().await;
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let connection = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let setup = setup_factory();
    let (first, second) = tokio::join!(
        setup(connection.clone(), Config { restricted: true }),
        setup(connection.clone(), Config { restricted: false })
    );
    assert_ne!(first.is_ok(), second.is_ok());
}

mod panicking {
    use super::*;
    fn factory() -> SetupFn {
        panic!("factory failed")
    }
    module_export_static! {
        setup_factory = factory, config = Config, worker_threads = 1,
        provides = ["ai.tinyhumans.tinybus.PanickingFactory"],
        methods = [], signals = [], requires = [], optional = [], lazy = false,
    }
}
#[test]
fn factory_panic_is_a_native_initialization_refusal() {
    let broker = Broker::new();
    let host = ModuleHost::new(broker);
    let error = host
        .attach_linked_with_config(
            panicking::linked_module().unwrap(),
            serde_json::json!({"restricted":true}),
        )
        .unwrap_err();
    assert!(matches!(error, tinybus::Error::ModuleRefused { .. }));
}
#[tokio::test]
async fn invalid_initial_configuration_does_not_construct_factory_state() {
    let _guard = TEST_LOCK.lock().await;
    let before = FACTORIES.load(Ordering::SeqCst);
    let host = ModuleHost::new(Broker::new());
    assert!(
        host.attach_linked_with_config(
            linked_module().unwrap(),
            serde_json::json!({"restricted":"invalid"})
        )
        .is_err()
    );
    assert_eq!(FACTORIES.load(Ordering::SeqCst), before);
}

#[test]
fn native_factory_starter_validates_host_before_constructing_state() {
    let code = unsafe {
        start_reconfigurable_module_with_factory::<Config, _, _>(
            std::ptr::null(),
            std::ptr::null_mut(),
            1,
            true,
            || {
                panic!("invalid host must be rejected before factory");
                #[allow(unreachable_code)]
                setup_factory()
            },
        )
    };
    assert_eq!(code, TB_BAD_ARGUMENT);
}
