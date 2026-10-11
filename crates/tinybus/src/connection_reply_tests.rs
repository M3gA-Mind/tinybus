//! Reply authorization regressions over broker-stamped memory deliveries.

use super::*;
use crate::broker::Broker;
use crate::transport::memory::MemoryBus;

#[tokio::test]
async fn another_broker_peer_cannot_complete_a_waiter_with_a_forged_reply() {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let responder = bus.connect().await.unwrap();
    let mut hello = Message::method_call(
        BusName::new(crate::BUS_NAME).unwrap(),
        ObjectPath::new(crate::BUS_PATH).unwrap(),
        InterfaceName::new(crate::BUS_INTERFACE).unwrap(),
        MemberName::new("Hello").unwrap(), serde_json::json!([]),
    );
    hello.header.serial = 1;
    responder.send(hello).await.unwrap();
    let responder_name = BusName::new(responder.recv().await.unwrap().unwrap().body.as_str().unwrap()).unwrap();
    let requester = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let forger = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let mut barriers = requester.add_match(MatchRule::new().signals()).await.unwrap();
    let request = Message::method_call(
        responder_name.clone(), ObjectPath::new("/fixture").unwrap(),
        InterfaceName::new("ai.tinyhumans.Fixture").unwrap(),
        MemberName::new("Await").unwrap(), serde_json::json!([]),
    );
    let pending = tokio::spawn({ let requester = requester.clone(); async move { requester.call_raw(request, DEFAULT_TIMEOUT).await } });
    let delivered = responder.recv().await.unwrap().unwrap();
    assert_eq!(delivered.header.sender, requester.unique_name());
    let mut forged = Message::method_return(&delivered.header, serde_json::json!("forged"));
    // Even a claimed B sender is overwritten to the actual peer C.
    forged.header.sender = Some(responder_name.clone());
    forger.send(forged).await.unwrap();
    forger.emit(ObjectPath::new("/fixture").unwrap(), InterfaceName::new("ai.tinyhumans.Fixture").unwrap(), MemberName::new("Barrier").unwrap(), ()).await.unwrap();
    let barrier = tokio::time::timeout(DEFAULT_TIMEOUT, barriers.recv()).await.unwrap().unwrap();
    assert_eq!(barrier.header.sender, forger.unique_name());
    // That connection's ordered signal arrived after its forged response, so
    // requester dispatch has processed the forgery before B is allowed to reply.
    let mut genuine = Message::method_return(&delivered.header, serde_json::json!("genuine"));
    genuine.header.serial = 2;
    responder.send(genuine).await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap(), serde_json::json!("genuine"));
}


fn fixture_call(destination: BusName) -> Message {
    Message::method_call(destination, ObjectPath::new("/fixture").unwrap(),
        InterfaceName::new("ai.tinyhumans.Fixture").unwrap(),
        MemberName::new("Await").unwrap(), serde_json::json!([]))
}

async fn raw_control(peer: &Arc<dyn Transport>, serial: u64, member: &str, body: Value) -> Message {
    let mut call = Message::method_call(BusName::new(crate::BUS_NAME).unwrap(),
        ObjectPath::new(crate::BUS_PATH).unwrap(),
        InterfaceName::new(crate::BUS_INTERFACE).unwrap(),
        MemberName::new(member).unwrap(), body);
    call.header.serial = serial;
    peer.send(call).await.unwrap();
    loop {
        let reply = peer.recv().await.unwrap().unwrap();
        if reply.header.reply_serial == Some(serial) { return reply; }
    }
}

async fn reject_forged_replies(named: bool, confidential: bool, genuine_error: bool) {
    let bus = MemoryBus::new();
    let broker = Broker::new();
    broker.clone().spawn(bus.clone());
    let responder = bus.connect().await.unwrap();
    let hello = raw_control(&responder, 1, "Hello", serde_json::json!([])).await;
    let responder_name = BusName::new(hello.body.as_str().unwrap()).unwrap();
    let alias = BusName::new("ai.tinyhumans.Fixture").unwrap();
    if named {
        let reply = raw_control(&responder, 2, "RequestName", serde_json::json!([alias])).await;
        assert_eq!(reply.header.kind, MessageKind::MethodReturn);
    }
    #[cfg(feature = "modules")]
    if confidential {
        broker.attest_module(&responder_name, crate::attest::Attestation {
            name: alias.clone(), sha256: "a".repeat(64),
        });
    }
    let requester = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let forger = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let mut barriers = requester.add_match(MatchRule::new().signals()).await.unwrap();
    let mut request = fixture_call(if named { alias } else { responder_name.clone() });
    request.header.confidential = confidential;
    let pending = tokio::spawn({ let requester = requester.clone(); async move {
        requester.call_raw(request, DEFAULT_TIMEOUT).await
    }});
    let delivered = responder.recv().await.unwrap().unwrap();
    if named { assert_eq!(delivered.header.expected_responder, Some(responder_name.clone())); }
    for error in [false, true] {
        for private in [false, true] {
            let mut forged = if error {
                Message::error_reply(&delivered.header, &Error::ConnectionClosed)
            } else {
                Message::method_return(&delivered.header, serde_json::json!("forged"))
            };
            forged.header.confidential = private;
            forged.header.sensitive = private;
            forged.header.sender = Some(BusName::new(crate::BUS_NAME).unwrap());
            forger.send(forged).await.unwrap();
        }
    }
    forger.emit(ObjectPath::new("/fixture").unwrap(),
        InterfaceName::new("ai.tinyhumans.Fixture").unwrap(),
        MemberName::new("Barrier").unwrap(), ()).await.unwrap();
    let barrier = tokio::time::timeout(DEFAULT_TIMEOUT, barriers.recv()).await.unwrap().unwrap();
    assert_eq!(barrier.header.sender, forger.unique_name());
    let mut genuine = if genuine_error {
        Message::error_reply(&delivered.header, &Error::UnknownMethod {
            interface: "ai.tinyhumans.Fixture".into(), member: "Await".into(),
        })
    } else {
        Message::method_return(&delivered.header, serde_json::json!("genuine"))
    };
    genuine.header.serial = 3;
    responder.send(genuine).await.unwrap();
    let result = pending.await.unwrap();
    if genuine_error {
        let error = result.unwrap_err();
        assert!(matches!(error, Error::MethodFailed { ref name, .. }
            if name == Error::UnknownMethod { interface: "ai.tinyhumans.Fixture".into(), member: "Await".into() }.wire_name()));
    } else { assert_eq!(result.unwrap(), serde_json::json!("genuine")); }
}

#[tokio::test]
async fn named_reply_rejects_forged_success_and_errors_with_any_private_flags() {
    reject_forged_replies(true, false, false).await;
}

#[tokio::test]
async fn unique_reply_preserves_genuine_error_after_rejecting_forged_errors() {
    reject_forged_replies(false, false, true).await;
}

#[cfg(feature = "modules")]
#[tokio::test]
async fn confidential_named_reply_binds_the_attested_owner() {
    reject_forged_replies(true, true, false).await;
}

#[cfg(feature = "modules")]
#[tokio::test]
async fn confidential_unique_reply_preserves_genuine_sanitized_error() {
    reject_forged_replies(false, true, true).await;
}

#[tokio::test]
async fn captured_alias_owner_is_checked_atomically_before_dispatch() {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let original = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let replacement = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let requester = Connection::connect(bus.connect().await.unwrap()).await.unwrap();
    let alias = BusName::new("ai.tinyhumans.Fixture").unwrap();
    original.request_name(alias.clone()).await.unwrap();
    let captured: BusName = serde_json::from_value(requester.call_bus(
        "ResolveCallTarget", serde_json::json!([alias])).await.unwrap()).unwrap();
    original.release_name(alias.clone()).await.unwrap();
    replacement.request_name(alias.clone()).await.unwrap();
    let mut call = fixture_call(alias.clone());
    call.header.expected_responder = Some(captured);
    // Send through the low-level port, bypassing the SDK's earlier check, to
    // pin the broker's routing check independently of requester validation.
    call.header.serial = 900;
    let mut replies = requester.add_match(MatchRule::new().signals()).await.unwrap();
    requester.send(call).await.unwrap();
    requester.emit(ObjectPath::new("/fixture").unwrap(),
        InterfaceName::new("ai.tinyhumans.Fixture").unwrap(),
        MemberName::new("Barrier").unwrap(), ()).await.unwrap();
    tokio::time::timeout(DEFAULT_TIMEOUT, replies.recv()).await.unwrap().unwrap();
    // Exercise the SDK pre-send check as well; the replacement must never be
    // invoked with a stale binding.
    let mut stale = fixture_call(alias.clone());
    stale.header.expected_responder = Some(original.unique_name().unwrap());
    assert!(matches!(requester.call_raw(stale, DEFAULT_TIMEOUT).await,
        Err(Error::NameHasNoOwner(name)) if name == alias));
}

#[tokio::test]
async fn raw_direct_reply_compatibility_does_not_claim_broker_identity() {
    let (a, b) = crate::transport::memory::pair(16);
    let requester = Connection::attach(Arc::new(a));
    let responder = Arc::new(b);
    let pending = tokio::spawn({ let requester = requester.clone(); async move {
        requester.call_raw(fixture_call(BusName::new("ai.tinyhumans.Fixture").unwrap()), DEFAULT_TIMEOUT).await
    }});
    let delivered = responder.recv().await.unwrap().unwrap();
    let mut response = Message::method_return(&delivered.header, serde_json::json!("raw"));
    response.header.serial = 1;
    responder.send(response).await.unwrap();
    assert_eq!(pending.await.unwrap().unwrap(), serde_json::json!("raw"));
}
