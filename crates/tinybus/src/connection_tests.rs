use super::*;
use crate::broker::Broker;
use crate::transport::memory::{MemoryBus, MemoryTransport};
use async_trait::async_trait;

/// A service that answers `Echo` and fails `Boom`.
struct Echo;

#[async_trait]
impl Interface for Echo {
    fn name(&self) -> InterfaceName {
        InterfaceName::new("ai.tinyhumans.Test").unwrap()
    }

    fn members(&self) -> Vec<MemberName> {
        vec![
            MemberName::new("Echo").unwrap(),
            MemberName::new("Boom").unwrap(),
            MemberName::new("Panic").unwrap(),
            MemberName::new("Hang").unwrap(),
            MemberName::new("Large").unwrap(),
        ]
    }

    async fn call(&self, member: &MemberName, args: Value) -> Result<Value> {
        match member.as_str() {
            "Echo" => Ok(args),
            "Boom" => Err(Error::failed("as requested")),
            "Panic" => panic!("secret panic payload"),
            "Hang" => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(Value::Null)
            }
            "Large" => Ok(Value::String(
                "x".repeat(crate::message::codec::MAX_FRAME_LEN + 1),
            )),
            other => Err(Error::failed(format!("unreachable: {other}"))),
        }
    }
}

fn path() -> ObjectPath {
    ObjectPath::new("/ai/tinyhumans/Test").unwrap()
}

fn call(member: &str, body: Value) -> Message {
    Message::method_call(
        BusName::new("ai.tinyhumans.Test").unwrap(),
        path(),
        InterfaceName::new("ai.tinyhumans.Test").unwrap(),
        MemberName::new(member).unwrap(),
        body,
    )
}

/// Two connections wired directly to each other, with no broker in the
/// middle: enough to exercise dispatch, replies, serials and timeouts.
async fn pair() -> (Connection, Connection) {
    let (a, b) = MemoryTransport::pair();
    let client = Connection::attach(Arc::new(a));
    let service = Connection::attach(Arc::new(b));
    service.serve_at(path(), Echo).await.unwrap();
    (client, service)
}

async fn brokered_pair() -> (Connection, Connection) {
    let transport = MemoryBus::new();
    Broker::new().spawn(transport.clone());
    let service = Connection::connect(transport.connect().await.unwrap())
        .await
        .unwrap();
    service.request_name("ai.tinyhumans.Test").await.unwrap();
    service.serve_at(path(), Echo).await.unwrap();
    let client = Connection::connect(transport.connect().await.unwrap())
        .await
        .unwrap();
    (client, service)
}

#[tokio::test]
async fn a_call_reaches_the_service_and_the_reply_comes_back() {
    let (client, _service) = pair().await;
    let reply = client
        .call_raw(call("Echo", serde_json::json!(["hi"])), DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!(["hi"]));
}

#[tokio::test]
async fn a_reply_larger_than_one_frame_arrives_through_a_bounded_stream() {
    let (client, _service) = brokered_pair().await;
    let proxy = client
        .proxy("ai.tinyhumans.Test", path().as_str(), "ai.tinyhumans.Test")
        .unwrap();
    let reply: String = proxy.call_streaming("Large", ()).await.unwrap();
    assert_eq!(reply.len(), crate::message::codec::MAX_FRAME_LEN + 1);
    assert!(reply.bytes().all(|byte| byte == b'x'));
}

#[tokio::test]
async fn a_streaming_caller_accepts_an_ordinary_legacy_reply() {
    let (client, _service) = pair().await;
    let reply: Vec<String> = client
        .decode_streaming_reply(serde_json::json!(["legacy"]), None)
        .await
        .unwrap();
    assert_eq!(reply, vec!["legacy"]);
}

#[tokio::test]
async fn a_streamed_reply_can_keep_progressing_past_the_reply_deadline() {
    let (client, service) = pair().await;
    let destination = BusName::new("ai.tinyhumans.Test").unwrap();
    let mut writer = service
        .open_stream(&destination, StreamDescriptor::with_len(6))
        .await
        .unwrap();
    let stream = writer.stream_ref();
    let send = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        writer.write(b"\"slow\"").await.unwrap();
        writer.finish().await.unwrap();
    });
    let reply = tokio::time::timeout(
        Duration::from_secs(1),
        client.decode_streaming_reply::<String>(
            serde_json::json!({ "$tinybus_stream_reply": stream }),
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    send.await.unwrap();
    assert_eq!(reply, "slow");
}

#[tokio::test]
async fn a_streamed_reply_that_stops_making_progress_times_out() {
    let (client, service) = pair().await;
    client.set_stream_limits(StreamLimits {
        idle_timeout: Duration::from_millis(20),
        ..StreamLimits::default()
    });
    let destination = BusName::new("ai.tinyhumans.Test").unwrap();
    let writer = service
        .open_stream(&destination, StreamDescriptor::with_len(6))
        .await
        .unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.decode_streaming_reply::<String>(
            serde_json::json!({ "$tinybus_stream_reply": writer.stream_ref() }),
            None,
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("went idle"), "{error}");
}

#[tokio::test]
async fn a_streaming_call_keeps_errors_as_ordinary_replies() {
    let (client, _service) = brokered_pair().await;
    let proxy = client
        .proxy("ai.tinyhumans.Test", path().as_str(), "ai.tinyhumans.Test")
        .unwrap();
    let error = proxy.call_streaming::<Value>("Boom", ()).await.unwrap_err();
    assert_eq!(error.wire_name(), Error::FAILED);
}

#[tokio::test]
async fn a_confidential_call_refuses_streamed_reply_semantics_before_send() {
    let (client, _service) = pair().await;
    let mut message = Message::streaming_call(
        BusName::new("ai.tinyhumans.Test").unwrap(),
        path(),
        InterfaceName::new("ai.tinyhumans.Test").unwrap(),
        MemberName::new("Echo").unwrap(),
        serde_json::json!(["secret"]),
    );
    message.header.confidential = true;
    let error = client.call_raw(message, DEFAULT_TIMEOUT).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("cannot request a streamed reply")
    );
}

#[tokio::test]
async fn a_failing_method_arrives_as_an_error_with_its_name_intact() {
    let (client, _service) = pair().await;
    let err = client
        .call_raw(call("Boom", serde_json::json!([])), DEFAULT_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(err.wire_name(), Error::FAILED);
    assert!(err.to_string().contains("as requested"), "{err}");
}

#[tokio::test]
async fn a_panicking_module_method_becomes_an_error_reply_rather_than_an_abort() {
    let (client, service) = pair().await;
    service.__set_panic_handler(Arc::new(|| Error::MethodFailed {
        name: "ai.tinyhumans.tinybus.Error.ModulePanicked".to_string(),
        message: "module panicked at fixture.rs:12:3".to_string(),
    }));
    let error = client
        .call_raw(call("Panic", serde_json::json!([])), DEFAULT_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(
        error.wire_name(),
        "ai.tinyhumans.tinybus.Error.ModulePanicked"
    );
    let reply = client
        .call_raw(call("Echo", serde_json::json!(["alive"])), DEFAULT_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!(["alive"]));
}

#[tokio::test]
async fn a_panic_reply_carries_the_location_but_never_the_payload() {
    let (client, service) = pair().await;
    service.__set_panic_handler(Arc::new(|| Error::MethodFailed {
        name: "ai.tinyhumans.tinybus.Error.ModulePanicked".to_string(),
        message: "module panicked at fixture.rs:12:3".to_string(),
    }));
    let error = client
        .call_raw(call("Panic", serde_json::json!([])), DEFAULT_TIMEOUT)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fixture.rs:12:3"));
    assert!(!error.to_string().contains("secret panic payload"));
}

#[tokio::test]
async fn an_unknown_member_is_reported_as_such_rather_than_hanging() {
    let (client, _service) = pair().await;
    let err = client
        .call_raw(call("Nope", serde_json::json!([])), DEFAULT_TIMEOUT)
        .await
        .unwrap_err();
    assert_eq!(err.wire_name(), Error::UNKNOWN_METHOD);
}

#[tokio::test]
async fn a_wedged_method_times_out_the_caller_and_not_the_connection() {
    let (client, _service) = pair().await;
    let err = client
        .call_raw(
            call("Hang", serde_json::json!([])),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err}");

    // The point of the exercise: the connection still works afterwards.
    let reply = client
        .call_raw(
            call("Echo", serde_json::json!(["still here"])),
            DEFAULT_TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!(["still here"]));
}

#[tokio::test]
async fn a_timed_out_call_does_not_leak_its_pending_slot() {
    let (client, _service) = pair().await;
    let _ = client
        .call_raw(
            call("Hang", serde_json::json!([])),
            Duration::from_millis(20),
        )
        .await;
    assert!(client.inner.pending.lock().await.is_empty());
}

#[tokio::test]
async fn concurrent_calls_are_matched_by_serial_not_by_order() {
    let (client, _service) = pair().await;
    let slow = client.call_raw(call("Echo", serde_json::json!(["first"])), DEFAULT_TIMEOUT);
    let fast = client.call_raw(call("Echo", serde_json::json!(["second"])), DEFAULT_TIMEOUT);
    let (a, b) = tokio::join!(slow, fast);
    assert_eq!(a.unwrap(), serde_json::json!(["first"]));
    assert_eq!(b.unwrap(), serde_json::json!(["second"]));
}

#[tokio::test]
async fn a_dropped_peer_wakes_waiters_instead_of_making_them_wait_out_the_clock() {
    let (a, b) = MemoryTransport::pair();
    let client = Connection::attach(Arc::new(a));
    drop(b);
    let err = client
        .call_raw(
            call("Echo", serde_json::json!([])),
            Duration::from_secs(300),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ConnectionClosed | Error::Transport(_)),
        "{err}"
    );
}

#[test]
fn a_scalar_argument_is_wrapped_into_the_positional_array() {
    assert_eq!(to_body(&"a").unwrap(), serde_json::json!(["a"]));
    assert_eq!(to_body(&("a", 1)).unwrap(), serde_json::json!(["a", 1]));
    assert_eq!(to_body(&Vec::<u8>::new()).unwrap(), serde_json::json!([]));
    assert_eq!(to_body(&Value::Null).unwrap(), serde_json::json!([]));
}

#[test]
fn a_rule_round_trips_through_its_wire_form() {
    let rule = MatchRule::parse(
        "type=signal,interface=ai.tinyhumans.Mail,member=Received,path_namespace=/ai/Mail",
    )
    .unwrap();
    assert_eq!(MatchRule::parse(&rule_to_wire(&rule)).unwrap(), rule);
}

#[tokio::test]
async fn broker_introspection_and_name_lifecycle_use_the_typed_helpers() {
    let transport = MemoryBus::new();
    Broker::new().spawn(transport.clone());
    let connection = Connection::connect(transport.connect().await.unwrap())
        .await
        .unwrap();
    let unique = connection.unique_name().unwrap();

    assert!(connection.list_names().await.unwrap().contains(&unique));
    assert_eq!(
        connection.name_owner(&unique).await.unwrap(),
        Some(unique.clone())
    );
    connection
        .request_name("ai.tinyhumans.TestService")
        .await
        .unwrap();
    assert_eq!(
        connection
            .name_owner("ai.tinyhumans.TestService")
            .await
            .unwrap(),
        Some(unique.clone())
    );
    connection
        .release_name("ai.tinyhumans.TestService")
        .await
        .unwrap();
    assert!(
        connection
            .name_owner("ai.tinyhumans.TestService")
            .await
            .unwrap()
            .is_none()
    );

    let manifest = PeerManifest::new("connection-test");
    connection.announce(&manifest).await.unwrap();
    assert_eq!(
        connection.manifest_of(&unique).await.unwrap(),
        Some(manifest)
    );
    assert_eq!(connection.peers().await.unwrap().len(), 1);
}

// The module-management methods they call exist only when the `modules`
// feature is compiled in; the portability jobs build without it.
#[cfg(feature = "modules")]
#[tokio::test]
async fn module_management_helpers_forward_their_requests_to_the_broker() {
    let transport = MemoryBus::new();
    Broker::new().spawn(transport.clone());
    let connection = Connection::connect(transport.connect().await.unwrap())
        .await
        .unwrap();

    assert!(connection.list_modules().await.is_err());
    assert!(connection.module("missing").await.is_err());
    assert!(connection.module_manifest("missing").await.is_err());
    assert!(connection.rescan_modules().await.is_err());
    assert!(
        connection
            .scan_modules([std::path::Path::new("/definitely/not/a/module")], true)
            .await
            .is_err()
    );
    assert!(
        connection
            .load_module("/definitely/not/a/module", serde_json::json!({}))
            .await
            .is_err()
    );
    assert!(
        connection
            .reinitialize_module("missing", serde_json::json!({ "secret": "redacted" }))
            .await
            .is_err()
    );
    assert!(
        connection
            .stop_module("missing", Duration::from_millis(1))
            .await
            .is_err()
    );
    assert!(connection.enable_module("missing", true).await.is_err());
}
