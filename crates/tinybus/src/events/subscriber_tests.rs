use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
struct TestEvent;

impl Event for TestEvent {
    fn domain(&self) -> &str {
        "test"
    }
}

struct Handler;

#[async_trait]
impl EventHandler<TestEvent> for Handler {
    fn name(&self) -> &str {
        "subscriber::test"
    }

    async fn handle(&self, _: &TestEvent) {}
}

#[test]
fn a_handler_without_a_filter_accepts_every_domain() {
    assert!(Handler.domains().is_none());
}

#[tokio::test]
async fn handlers_and_closures_can_be_called_through_the_trait() {
    Handler.handle(&TestEvent).await;
    let closure = FnSubscriber {
        name: "closure".to_string(),
        handler: |_| async {},
        _event: std::marker::PhantomData::<fn() -> TestEvent>,
    };
    assert_eq!(closure.name(), "closure");
    closure.handle(&TestEvent).await;
}

#[tokio::test]
async fn a_handle_exposes_its_name_and_cancels_its_task() {
    let task = tokio::spawn(std::future::pending::<()>());
    let handle = SubscriptionHandle::new("test::pending".to_string(), task);
    assert_eq!(handle.name(), "test::pending");
    handle.cancel();
}

#[tokio::test]
async fn forgetting_a_completed_handle_leaves_its_task_alone() {
    let task = tokio::spawn(async {});
    tokio::task::yield_now().await;
    SubscriptionHandle::new("test::permanent".to_string(), task).forget();
}

#[tokio::test]
async fn a_closed_signal_stream_ends_the_dispatch_loop() {
    let (sender, receiver) = broadcast::channel(1);
    drop(sender);
    let config = EventBusConfig::new("/events", "ai.tinyhumans.Events").unwrap();
    let mut handle = spawn(receiver, config, Arc::new(Handler));
    assert_eq!(handle.name(), "subscriber::test");
    (&mut handle.task).await.unwrap();
}

#[tokio::test]
async fn malformed_and_lagged_messages_do_not_end_the_dispatch_loop() {
    let (sender, receiver) = broadcast::channel(1);
    let config = EventBusConfig::new("/events", "ai.tinyhumans.Events").unwrap();
    let malformed = Message::signal(
        crate::ObjectPath::new("/events/test").unwrap(),
        crate::InterfaceName::new("ai.tinyhumans.Other").unwrap(),
        crate::MemberName::new("Published").unwrap(),
        serde_json::json!([TestEvent]),
    );
    let valid = Message::signal(
        crate::ObjectPath::new("/events/test").unwrap(),
        crate::InterfaceName::new("ai.tinyhumans.Events").unwrap(),
        crate::MemberName::new("Published").unwrap(),
        serde_json::json!([TestEvent]),
    );
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::channel(1);
    sender.send(malformed).unwrap();
    sender.send(valid.clone()).unwrap();
    sender.send(valid).unwrap();
    let handler = FnSubscriber {
        name: "subscriber::test".to_string(),
        handler: move |_| {
            let seen_tx = seen_tx.clone();
            async move { seen_tx.send(()).await.unwrap() }
        },
        _event: std::marker::PhantomData::<fn() -> TestEvent>,
    };
    let handle = spawn(receiver, config, Arc::new(handler));
    assert_eq!(handle.name(), "subscriber::test");
    tokio::time::timeout(std::time::Duration::from_secs(1), seen_rx.recv())
        .await
        .unwrap()
        .unwrap();
    drop(handle);
}
