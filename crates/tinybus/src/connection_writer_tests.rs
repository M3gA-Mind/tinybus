//! A message the transport refuses must not silence the connection.

use super::*;
use crate::broker::Broker;
use crate::message::codec::MAX_FRAME_LEN;
use crate::transport::unix::{UnixListenerAdapter, UnixTransport};

struct Echo;

#[async_trait::async_trait]
impl Interface for Echo {
    fn name(&self) -> InterfaceName {
        InterfaceName::new("test.Echo").unwrap()
    }

    fn members(&self) -> Vec<MemberName> {
        vec![MemberName::new("Echo").unwrap()]
    }

    async fn call(&self, _member: &MemberName, args: Value) -> Result<Value> {
        Ok(args)
    }
}

/// Two peers on a real socket bus, where the frame cap is enforced on write.
async fn peers() -> (tempfile::TempDir, Connection, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bus");
    let listener = UnixListenerAdapter::bind(&path).await.unwrap();
    Broker::new().spawn(listener);
    let client = Connection::connect(Box::new(UnixTransport::connect(&path).await.unwrap()))
        .await
        .unwrap();
    let service = Connection::connect(Box::new(UnixTransport::connect(&path).await.unwrap()))
        .await
        .unwrap();
    service
        .serve_at(ObjectPath::new("/test/echo").unwrap(), Echo)
        .await
        .unwrap();
    service.request_name("test.Echo").await.unwrap();
    (dir, client, service)
}

fn echo(client: &Connection) -> Proxy {
    client
        .proxy("test.Echo", "/test/echo", "test.Echo")
        .unwrap()
        .with_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn a_call_over_the_frame_cap_fails_at_once_and_the_connection_keeps_working() {
    let (_dir, client, _service) = peers().await;
    let proxy = echo(&client);

    let huge = "x".repeat(MAX_FRAME_LEN + 1);
    let started = std::time::Instant::now();
    let refused = proxy.call::<Value>("Echo", (huge,)).await.unwrap_err();
    // Answered with the refusal, not left to run into the call's own timeout.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    eprintln!("REFUSED: {refused:?}");
    match refused {
        Error::MethodFailed { name, message } => {
            assert_eq!(name, "ai.tinyhumans.tinybus.Error.Protocol");
            assert!(message.contains("exceeds"), "{message}");
        }
        other => panic!("expected the refusal, got {other:?}"),
    }

    // The writer survived: the next calls go out and come back.
    for n in 0..3 {
        let echoed: Value = proxy.call("Echo", (n,)).await.unwrap();
        assert_eq!(echoed, serde_json::json!([n]));
    }
}

#[tokio::test]
async fn a_service_keeps_replying_after_one_of_its_messages_was_refused() {
    let (_dir, client, service) = peers().await;
    // The service's own writer takes the refusal: a signal over the cap.
    let _ = service
        .emit(
            ObjectPath::new("/test/echo").unwrap(),
            InterfaceName::new("test.Echo").unwrap(),
            MemberName::new("Big").unwrap(),
            "x".repeat(MAX_FRAME_LEN + 1),
        )
        .await;
    let echoed: Value = echo(&client).call("Echo", (7,)).await.unwrap();
    assert_eq!(echoed, serde_json::json!([7]));
}
