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

#[tokio::test(flavor = "current_thread")]
async fn cancelling_receive_and_later_raw_injection_cannot_relabel_queued_broker_delivery() {
    let bus = MemoryBus::new();
    let broker = crate::broker::Broker::new();
    broker.spawn(bus.clone());
    let (peer, broker_end) = MemoryTransport::pair();
    let broker_end = Arc::new(broker_end);
    broker.attach(broker_end.clone());
    let receiver = Connection::connect(Box::new(peer)).await.unwrap();
    let mut deliveries = receiver.signal_deliveries();
    let mut legacy = receiver.signals();
    receiver
        .add_match(
            MatchRule::new()
                .signals()
                .interface(InterfaceName::new("ai.tinyhumans.Events").unwrap()),
        )
        .await
        .unwrap();
    {
        let mut pending = std::pin::pin!(deliveries.recv());
        std::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    let publisher = connect(&bus).await;
    publisher.send(signal("verified")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), legacy.recv())
        .await
        .unwrap()
        .unwrap();
    let mut forged = signal("unverified");
    forged.header.sender = publisher.unique_name();
    broker_end.send(forged).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), legacy.recv())
        .await
        .unwrap()
        .unwrap();
    let verified = next_delivery(&mut deliveries).await;
    let unverified = next_delivery(&mut deliveries).await;
    assert_eq!(verified.context().origin(), SignalOrigin::Brokered);
    assert_eq!(
        verified.context().authenticated_sender(),
        publisher.unique_name().as_ref()
    );
    assert_eq!(unverified.context().origin(), SignalOrigin::Unverified);
    assert!(unverified.context().authenticated_sender().is_none());
    let mut editable = verified.message().clone();
    editable.header.sender = None;
    assert_eq!(
        verified.context().authenticated_sender(),
        publisher.unique_name().as_ref()
    );
    assert_eq!(verified.message().header.sender, publisher.unique_name());
}

struct AdmittedLocal {
    publisher: crate::LocalPublisherIdentity,
    accepted: mpsc::UnboundedSender<String>,
    refused: mpsc::UnboundedSender<()>,
}
#[async_trait]
impl EventHandler<ClaimedEvent> for AdmittedLocal {
    fn name(&self) -> &str {
        "admitted-local"
    }
    async fn handle(&self, _: &ClaimedEvent) {
        panic!("missing context");
    }
    async fn handle_with_context(&self, event: &ClaimedEvent, context: &SignalContext) {
        if context.local_publisher() == Some(&self.publisher) {
            self.accepted.send(event.tenant.clone()).unwrap();
        } else {
            self.refused.send(()).unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_admitted_local_handler_refuses_identical_remote_payload_and_only_accepts_bound_publisher()
 {
    let bus = MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let local = connect(&bus).await;
    let remote = connect(&bus).await;
    let events = EventBus::<ClaimedEvent>::attach(local.clone(), event_config())
        .await
        .unwrap();
    let (accepted, mut accepted_rx) = mpsc::unbounded_channel();
    let (refused, mut refused_rx) = mpsc::unbounded_channel();
    let _subscription = events.subscribe(Arc::new(AdmittedLocal {
        publisher: local.local_publisher_identity(),
        accepted,
        refused,
    }));
    remote.send(signal("same-claimed-tenant")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), refused_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(accepted_rx.try_recv().is_err());
    local.deliver_local(signal("same-claimed-tenant"));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), accepted_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        "same-claimed-tenant"
    );
}

fn watch_rule() -> crate::MatchRule {
    crate::MatchRule::new()
        .signals()
        .interface(InterfaceName::new("ai.tinyhumans.Events").unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn emit_to_reaches_only_the_addressed_subscriber() {
    let bus = MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let publisher = connect(&bus).await;
    let target = connect(&bus).await;
    let bystander = connect(&bus).await;
    let mut target_rx = target.add_match(watch_rule()).await.unwrap();
    let mut bystander_rx = bystander.add_match(watch_rule()).await.unwrap();

    publisher
        .emit_to(
            target.unique_name().unwrap(),
            ObjectPath::new("/events/artifact").unwrap(),
            InterfaceName::new("ai.tinyhumans.Events").unwrap(),
            MemberName::new("Published").unwrap(),
            ["private"],
        )
        .await
        .unwrap();
    // A later broadcast proves the bystander is subscribed and listening.
    publisher
        .emit(
            ObjectPath::new("/events/artifact").unwrap(),
            InterfaceName::new("ai.tinyhumans.Events").unwrap(),
            MemberName::new("Published").unwrap(),
            ["public"],
        )
        .await
        .unwrap();

    let first = tokio::time::timeout(Duration::from_secs(2), target_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.body, serde_json::json!(["private"]));
    let seen = tokio::time::timeout(Duration::from_secs(2), bystander_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.body, serde_json::json!(["public"]));
}

#[tokio::test(flavor = "current_thread")]
async fn emit_to_still_requires_the_target_to_have_subscribed() {
    let bus = MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let publisher = connect(&bus).await;
    let target = connect(&bus).await;
    let mut rx = target.signals();
    publisher
        .emit_to(
            target.unique_name().unwrap(),
            ObjectPath::new("/events/artifact").unwrap(),
            InterfaceName::new("ai.tinyhumans.Events").unwrap(),
            MemberName::new("Published").unwrap(),
            ["x"],
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err()
    );
}

#[test]
fn a_signal_addressed_to_a_well_known_name_is_refused() {
    let message = Message::signal_to(
        BusName::new("ai.tinyhumans.Storage").unwrap(),
        ObjectPath::new("/events/artifact").unwrap(),
        InterfaceName::new("ai.tinyhumans.Events").unwrap(),
        MemberName::new("Published").unwrap(),
        serde_json::json!([]),
    );
    assert!(message.validate().is_err());
}
