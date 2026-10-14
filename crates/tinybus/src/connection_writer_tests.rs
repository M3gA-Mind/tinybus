//! A message the transport refuses must not silence the connection.

use super::*;
use crate::broker::Broker;
use crate::transport::memory::MemoryBus;

/// Refuses any message addressed to `TooBig`, as a socket transport refuses a
/// frame over the cap: with a protocol error, before anything is written.
struct Refusing(Box<dyn Transport>);

#[async_trait::async_trait]
impl Transport for Refusing {
    async fn send(&self, message: Message) -> Result<()> {
        if message.header.member.as_ref().map(MemberName::as_str) == Some("TooBig") {
            return Err(Error::protocol("frame exceeds the size cap"));
        }
        self.0.send(message).await
    }

    async fn recv(&self) -> Result<Option<Message>> {
        self.0.recv().await
    }

    async fn close(&self) -> Result<()> {
        self.0.close().await
    }
}

struct Echo;

#[async_trait::async_trait]
impl Interface for Echo {
    fn name(&self) -> InterfaceName {
        InterfaceName::new("test.Echo").unwrap()
    }

    fn members(&self) -> Vec<MemberName> {
        ["Echo", "TooBig"]
            .map(|member| MemberName::new(member).unwrap())
            .to_vec()
    }

    async fn call(&self, _member: &MemberName, args: Value) -> Result<Value> {
        Ok(args)
    }
}

async fn peers(refuse_on_client: bool) -> (Connection, Connection) {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let link = bus.connect().await.unwrap();
    let link: Box<dyn Transport> = if refuse_on_client {
        Box::new(Refusing(link))
    } else {
        link
    };
    let client = Connection::connect(link).await.unwrap();
    let service = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    service
        .serve_at(ObjectPath::new("/test/echo").unwrap(), Echo)
        .await
        .unwrap();
    service.request_name("test.Echo").await.unwrap();
    (client, service)
}

fn echo(client: &Connection) -> Proxy {
    client
        .proxy("test.Echo", "/test/echo", "test.Echo")
        .unwrap()
        .with_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn a_call_the_transport_refuses_fails_at_once_and_the_connection_keeps_working() {
    let (client, _service) = peers(true).await;
    let proxy = echo(&client);

    let started = std::time::Instant::now();
    let refused = proxy.call::<Value>("TooBig", (1,)).await.unwrap_err();
    // Answered with the refusal, not left to run into the call's own timeout.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    match refused {
        Error::MethodFailed { name, message } => {
            assert_eq!(name, "ai.tinyhumans.tinybus.Error.Protocol");
            assert!(message.contains("size cap"), "{message}");
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
async fn a_refused_signal_does_not_stop_later_calls() {
    let (client, _service) = peers(true).await;
    // A refused message that is not a call has no caller to answer; the writer
    // must still carry on.
    client
        .emit(
            ObjectPath::new("/test/echo").unwrap(),
            InterfaceName::new("test.Echo").unwrap(),
            MemberName::new("TooBig").unwrap(),
            1,
        )
        .await
        .unwrap();
    let echoed: Value = echo(&client).call("Echo", (7,)).await.unwrap();
    assert_eq!(echoed, serde_json::json!([7]));
}
