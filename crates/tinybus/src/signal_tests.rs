use super::*;

#[test]
fn local_identity_is_stable_for_clones_and_distinct_for_new_connections() {
    let first = LocalPublisherIdentity::new();
    assert_eq!(first, first.clone());
    assert_ne!(first, LocalPublisherIdentity::new());
    assert_eq!(format!("{first:?}"), "LocalPublisherIdentity { .. }");
    let mut set = std::collections::HashSet::new();
    set.insert(first.clone());
    assert!(set.contains(&first));
}

#[test]
fn default_context_has_no_verified_authority() {
    let context = SignalContext::default();
    assert_eq!(context.origin(), SignalOrigin::Unverified);
    assert!(context.authenticated_sender().is_none());
    assert!(context.local_publisher().is_none());
}

#[test]
fn even_verified_delivery_does_not_authenticate_missing_or_well_known_sender() {
    for sender in [None, Some(BusName::new("ai.tinyhumans.Forged").unwrap())] {
        let mut message = Message::signal(
            crate::ObjectPath::new("/events").unwrap(),
            crate::InterfaceName::new("ai.tinyhumans.Events").unwrap(),
            crate::MemberName::new("Published").unwrap(),
            serde_json::Value::Null,
        );
        message.header.sender = sender;
        let delivery = SignalDelivery::received(message, true);
        assert_eq!(delivery.context().origin(), SignalOrigin::Brokered);
        assert!(delivery.context().authenticated_sender().is_none());
        assert!(delivery.context().local_publisher().is_none());
    }
}
