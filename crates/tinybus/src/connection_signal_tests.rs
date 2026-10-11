use super::*;
use crate::{
    SignalContext, SignalOrigin,
    events::{Event, EventBus, EventBusConfig, EventHandler},
    transport::memory::{MemoryBus, MemoryTransport},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct ClaimedEvent {
    tenant: String,
}
impl Event for ClaimedEvent {
    fn domain(&self) -> &str {
        "artifact"
    }
}

fn event_config() -> EventBusConfig {
    EventBusConfig::new("/events", "ai.tinyhumans.Events").unwrap()
}
fn signal(tenant: &str) -> Message {
    Message::signal(
        ObjectPath::new("/events/artifact").unwrap(),
        InterfaceName::new("ai.tinyhumans.Events").unwrap(),
        MemberName::new("Published").unwrap(),
        serde_json::json!([ClaimedEvent {
            tenant: tenant.into()
        }]),
    )
}
async fn next_delivery(
    rx: &mut broadcast::Receiver<crate::SignalDelivery>,
) -> crate::SignalDelivery {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn connect(bus: &MemoryBus) -> Connection {
    Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap()
}
struct Capture(mpsc::UnboundedSender<(String, SignalContext)>);
#[async_trait]
impl EventHandler<ClaimedEvent> for Capture {
    fn name(&self) -> &str {
        "provenance"
    }
    async fn handle(&self, _: &ClaimedEvent) {
        panic!("context extension was bypassed");
    }
    async fn handle_with_context(&self, event: &ClaimedEvent, context: &SignalContext) {
        self.0
            .send((event.tenant.clone(), context.clone()))
            .unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn local_signal_captures_exact_publisher_identity_not_header_or_tenant() {
    let bus = MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let connection = connect(&bus).await;
    let other = connect(&bus).await;
    let events = EventBus::<ClaimedEvent>::without_match(connection.clone(), event_config());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _subscription = events.subscribe(Arc::new(Capture(tx)));
    let mut message = signal("claimed-foreign-tenant");
    message.header.sender = other.unique_name();
    connection.deliver_local(message);
    let (claimed, context) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed, "claimed-foreign-tenant");
    assert_eq!(context.origin(), SignalOrigin::Local);
    assert_eq!(
        context.local_publisher(),
        Some(&connection.local_publisher_identity())
    );
    assert_ne!(
        context.local_publisher(),
        Some(&other.local_publisher_identity())
    );
    assert!(context.authenticated_sender().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn typed_broker_signal_keeps_authenticated_original_publisher_through_queue() {
    let bus = MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let first = connect(&bus).await;
    let second = connect(&bus).await;
    let receiver = connect(&bus).await;
    let events = EventBus::<ClaimedEvent>::attach(receiver, event_config())
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _subscription = events.subscribe(Arc::new(Capture(tx)));
    for (publisher, claimed) in [(&first, "first"), (&second, "second")] {
        let mut message = signal(claimed);
        message.header.sender = Some(BusName::new(":1.99999").unwrap());
        publisher.send(message).await.unwrap();
    }
    let mut captured = Vec::new();
    for _ in 0..2 {
        captured.push(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    for (publisher, claimed) in [(&first, "first"), (&second, "second")] {
        let (_, context) = captured
            .iter()
            .find(|(tenant, _)| tenant == claimed)
            .unwrap();
        assert_eq!(context.origin(), SignalOrigin::Brokered);
        assert_eq!(
            context.authenticated_sender(),
            publisher.unique_name().as_ref()
        );
        assert!(context.local_publisher().is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn raw_peer_signal_with_forged_sender_has_no_verified_identity() {
    let (sender, receiver) = MemoryTransport::pair();
    let connection = Connection::attach(Arc::new(receiver));
    let mut deliveries = connection.signal_deliveries();
    let mut message = signal("forged");
    message.header.sender = Some(BusName::new(":1.42").unwrap());
    sender.send(message).await.unwrap();
    let received = next_delivery(&mut deliveries).await;
    assert_eq!(
        received.message().header.sender.as_ref().unwrap().as_str(),
        ":1.42"
    );
    assert_eq!(received.context().origin(), SignalOrigin::Unverified);
    assert!(received.context().authenticated_sender().is_none());
    assert!(received.context().local_publisher().is_none());
}

struct WrappedTransport(MemoryTransport);
#[async_trait]
impl Transport for WrappedTransport {
    async fn send(&self, message: Message) -> Result<()> {
        self.0.send(message).await
    }
    async fn recv(&self) -> Result<Option<Message>> {
        self.0.recv().await
    }
    async fn close(&self) -> Result<()> {
        self.0.close().await
    }
    fn describe(&self) -> String {
        "memory".into()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn custom_transport_and_successful_hello_cannot_borrow_broker_signal_proof() {
    let bus = MemoryBus::new();
    let broker = crate::broker::Broker::new();
    broker.spawn(bus.clone());
    let (peer, broker_end) = MemoryTransport::pair();
    broker.attach(Arc::new(broker_end));
    let receiver = Connection::connect(Box::new(WrappedTransport(peer)))
        .await
        .unwrap();
    assert!(receiver.unique_name().is_some());
    let mut deliveries = receiver.signal_deliveries();
    receiver
        .add_match(
            MatchRule::new()
                .signals()
                .interface(InterfaceName::new("ai.tinyhumans.Events").unwrap()),
        )
        .await
        .unwrap();
    let publisher = connect(&bus).await;
    publisher.send(signal("forged-authority")).await.unwrap();
    let received = next_delivery(&mut deliveries).await;
    assert_eq!(received.message().header.sender, publisher.unique_name());
    assert_eq!(received.context().origin(), SignalOrigin::Unverified);
    assert!(received.context().authenticated_sender().is_none());
    assert!(received.context().local_publisher().is_none());
    receiver.deliver_local(signal("local-native"));
    let local = next_delivery(&mut deliveries).await;
    assert_eq!(local.context().origin(), SignalOrigin::Local);
    assert!(local.context().authenticated_sender().is_none());
    assert_eq!(
        local.context().local_publisher(),
        Some(&receiver.local_publisher_identity())
    );
}
