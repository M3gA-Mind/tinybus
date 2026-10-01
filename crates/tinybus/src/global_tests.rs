use super::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Tick(u32);

impl Event for Tick {
    fn domain(&self) -> &str {
        "test"
    }
}

fn config() -> EventBusConfig {
    EventBusConfig::new("/ai/tinyhumans/test/events", "ai.tinyhumans.test.Events").unwrap()
}

struct Capture(Arc<Mutex<Vec<Tick>>>);

#[async_trait]
impl EventHandler<Tick> for Capture {
    fn name(&self) -> &str {
        "test::capture"
    }
    async fn handle(&self, event: &Tick) {
        self.0.lock().await.push(event.clone());
    }
}

#[tokio::test]
async fn publishing_before_init_is_a_no_op_rather_than_a_panic() {
    // Early-startup publishes and bus-less unit tests both depend on this.
    let bus: OnceBus<Tick> = OnceBus::new();
    assert!(!bus.is_initialised());
    bus.publish(Tick(1));
    assert!(
        bus.subscribe(Arc::new(Capture(Arc::new(Mutex::new(Vec::new())))))
            .is_none()
    );
}

#[tokio::test]
async fn an_in_process_bus_delivers_end_to_end() {
    let bus: OnceBus<Tick> = OnceBus::new();
    bus.init_in_process(config()).await.unwrap();
    assert!(bus.is_initialised());

    let seen = Arc::new(Mutex::new(Vec::new()));
    let _handle = bus.subscribe(Arc::new(Capture(seen.clone()))).unwrap();
    bus.publish(Tick(7));

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while seen.lock().await.is_empty() {
        assert!(tokio::time::Instant::now() < deadline, "no delivery");
        tokio::task::yield_now().await;
    }
    assert_eq!(seen.lock().await[0], Tick(7));
}

#[tokio::test]
async fn a_second_init_returns_the_first_bus() {
    // Two subsystems both initialising at startup is normal; the second
    // silently replacing the first would strand every existing subscriber.
    let bus: OnceBus<Tick> = OnceBus::new();
    let first = bus.init_in_process(config()).await.unwrap() as *const _;
    let second = bus.init_in_process(config()).await.unwrap() as *const _;
    assert_eq!(first, second);
}

#[test]
fn the_native_registry_works_without_a_runtime_or_an_initialised_bus() {
    // Startup registers handlers from sync contexts, before anything async
    // exists. This is a `#[test]`, so it has no runtime at all.
    let bus: OnceBus<Tick> = OnceBus::new();
    bus.native()
        .register::<u32, u32, _, _>("test.double", |n| async move { Ok(n * 2) });
    assert!(bus.native().is_registered("test.double"));
    assert!(!bus.is_initialised());
}

#[test]
fn a_once_bus_can_be_a_static() {
    // `new()` being `const` is what lets the host declare the singleton.
    static BUS: OnceBus<Tick> = OnceBus::new();
    assert!(!BUS.is_initialised());
}

#[test]
fn default_constructs_the_same_uninitialised_singleton() {
    let bus = OnceBus::<Tick>::default();
    assert!(bus.get().is_none());
    assert!(!bus.is_initialised());
}

#[tokio::test]
async fn an_existing_transport_initialises_and_announces_a_manifest() {
    let transport = MemoryBus::new();
    Broker::new().spawn(transport.clone());
    let bus: OnceBus<Tick> = OnceBus::new();

    let initialised = bus
        .init_over(transport.connect().await.unwrap(), config())
        .await
        .unwrap();
    assert_eq!(
        initialised.config().interface.as_str(),
        "ai.tinyhumans.test.Events"
    );
    assert!(std::ptr::eq(bus.get().unwrap(), initialised));

    let manifest = PeerManifest::new("test-host").version(crate::Version::parse("1.0.0").unwrap());
    bus.announce(&manifest).await.unwrap();
}

#[tokio::test]
async fn announcing_before_initialisation_is_a_safe_no_op() {
    let bus: OnceBus<Tick> = OnceBus::new();
    let manifest = PeerManifest::new("test-host").version(crate::Version::parse("1.0.0").unwrap());
    bus.announce(&manifest).await.unwrap();
}
