use super::*;
use crate::name::{BusName, InterfaceName, MemberName, ObjectPath};

fn message(member: &str) -> Message {
    Message::method_call(
        BusName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        ObjectPath::new("/ai/tinyhumans/openhuman/Voice").unwrap(),
        InterfaceName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        MemberName::new(member).unwrap(),
        serde_json::Value::Null,
    )
}

#[tokio::test]
async fn a_pair_delivers_in_order_in_both_directions() {
    let (a, b) = MemoryTransport::pair();
    a.send(message("First")).await.unwrap();
    a.send(message("Second")).await.unwrap();
    b.send(message("Back")).await.unwrap();

    assert_eq!(
        b.recv()
            .await
            .unwrap()
            .unwrap()
            .header
            .member
            .unwrap()
            .as_str(),
        "First"
    );
    assert_eq!(
        b.recv()
            .await
            .unwrap()
            .unwrap()
            .header
            .member
            .unwrap()
            .as_str(),
        "Second"
    );
    assert_eq!(
        a.recv()
            .await
            .unwrap()
            .unwrap()
            .header
            .member
            .unwrap()
            .as_str(),
        "Back"
    );
}

#[tokio::test]
async fn a_dropped_peer_reads_as_clean_shutdown_not_as_an_error() {
    let (a, b) = MemoryTransport::pair();
    drop(b);
    assert!(a.recv().await.unwrap().is_none());
    // ...but writing to a hung-up peer is a real failure the caller must see.
    assert!(a.send(message("Late")).await.is_err());
}

#[tokio::test]
async fn connecting_hands_the_far_end_to_the_listener() {
    let bus = MemoryBus::new();
    let peer = bus.connect().await.unwrap();
    let accepted = bus.accept().await.unwrap().expect("a peer");
    peer.send(message("Hello")).await.unwrap();
    assert_eq!(
        accepted
            .recv()
            .await
            .unwrap()
            .unwrap()
            .header
            .member
            .unwrap()
            .as_str(),
        "Hello"
    );
}

#[tokio::test]
async fn sending_on_a_closed_end_is_a_transport_error() {
    let (a, _b) = MemoryTransport::pair();
    a.close().await.unwrap();
    // Closing twice is a normal shutdown race, not an error.
    a.close().await.unwrap();
    let err = a.send(message("Late")).await.unwrap_err();
    assert!(err.to_string().contains("closed"), "{err}");
}

#[tokio::test]
async fn sending_to_a_dropped_peer_is_a_transport_error() {
    let (a, b) = MemoryTransport::pair();
    drop(b);
    let err = a.send(message("Orphan")).await.unwrap_err();
    assert!(err.to_string().contains("dropped"), "{err}");
}

#[test]
fn a_bus_describes_itself_as_the_memory_listener() {
    assert_eq!(Listener::describe(&MemoryBus::new()), "memory");
}
