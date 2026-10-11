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
