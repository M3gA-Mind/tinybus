use super::*;

fn call() -> Message {
    Message::method_call(
        BusName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        ObjectPath::new("/ai/tinyhumans/openhuman/Voice").unwrap(),
        InterfaceName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        MemberName::new("Transcribe").unwrap(),
        serde_json::json!(["/tmp/clip.wav"]),
    )
}

#[test]
fn a_reply_goes_back_to_the_senders_unique_name() {
    let mut c = call();
    c.header.serial = 9;
    c.header.sender = Some(BusName::new(":1.3").unwrap());
    let reply = Message::method_return(&c.header, serde_json::json!("hello"));
    assert_eq!(reply.header.reply_serial, Some(9));
    assert_eq!(
        reply.header.destination,
        Some(BusName::new(":1.3").unwrap())
    );
}

#[test]
fn an_error_reply_keeps_the_dotted_name_matchable() {
    let c = call();
    let err = Error::UnknownMethod {
        interface: InterfaceName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        member: MemberName::new("Nope").unwrap(),
    };
    let reply = Message::error_reply(&c.header, &err);
    assert_eq!(
        reply.header.error_name.as_deref(),
        Some(Error::UNKNOWN_METHOD)
    );
    assert_eq!(reply.into_error().wire_name(), Error::UNKNOWN_METHOD);
}

#[test]
fn a_call_without_a_destination_is_refused_on_ingress() {
    let mut c = call();
    c.header.destination = None;
    let err = c.validate().unwrap_err();
    assert!(err.to_string().contains("no destination"), "{err}");
}

#[test]
fn a_signal_needs_no_destination() {
    let sig = Message::signal(
        ObjectPath::new("/ai/tinyhumans/openhuman/Mail").unwrap(),
        InterfaceName::new("ai.tinyhumans.openhuman.Mail").unwrap(),
        MemberName::new("Received").unwrap(),
        serde_json::json!([{ "id": "abc" }]),
    );
    sig.validate().unwrap();
    assert!(sig.header.destination.is_none());
}

#[test]
fn the_header_omits_absent_fields_rather_than_writing_nulls() {
    // Signals outnumber every other message on a busy bus; not writing
    // seven `null`s per signal is most of the framing cost.
    let sig = Message::signal(
        ObjectPath::root(),
        InterfaceName::new("ai.tinyhumans.Test").unwrap(),
        MemberName::new("Tick").unwrap(),
        Value::Null,
    );
    let json = serde_json::to_string(&sig).unwrap();
    assert!(!json.contains("null"), "{json}");
    assert!(!json.contains("destination"), "{json}");
}

#[test]
fn a_signal_cannot_be_confidential_because_it_is_a_broadcast() {
    let mut sig = Message::signal(
        ObjectPath::root(),
        InterfaceName::new("ai.tinyhumans.Test").unwrap(),
        MemberName::new("Tick").unwrap(),
        Value::Null,
    );
    sig.header.confidential = true;
    let err = sig.validate().unwrap_err();
    assert!(err.to_string().contains("broadcast"), "{err}");
}

#[test]
fn only_a_method_call_can_request_a_streamed_reply() {
    let mut signal = Message::signal(
        ObjectPath::new("/events").unwrap(),
        InterfaceName::new("ai.tinyhumans.Events").unwrap(),
        MemberName::new("Published").unwrap(),
        Value::Null,
    );
    signal.header.stream_reply = true;
    assert!(signal.validate().is_err());

    let call = Message::streaming_call(
        BusName::new("ai.tinyhumans.Service").unwrap(),
        ObjectPath::new("/service").unwrap(),
        InterfaceName::new("ai.tinyhumans.Service").unwrap(),
        MemberName::new("Export").unwrap(),
        Value::Null,
    );
    call.validate().unwrap();
}

#[test]
fn a_confidential_message_without_a_destination_is_refused_on_ingress() {
    let mut c = call();
    c.header.confidential = true;
    c.header.destination = None;
    assert!(c.validate().is_err());
}

#[test]
fn a_reply_inherits_confidentiality_and_an_error_reply_never_does() {
    // A key-derivation call answers with a key. A reply that quietly lost
    // the flag would leak on the way back what the call protected on the
    // way out.
    let mut c = Message::confidential_call(
        BusName::new("ai.tinyhumans.openhuman.Wallet").unwrap(),
        ObjectPath::new("/ai/tinyhumans/openhuman/Wallet").unwrap(),
        InterfaceName::new("ai.tinyhumans.openhuman.Wallet").unwrap(),
        MemberName::new("DeriveKey").unwrap(),
        serde_json::json!([]),
    );
    c.header.sender = Some(BusName::new(":1.3").unwrap());
    assert!(c.header.confidential);
    c.validate().unwrap();

    assert!(
        Message::method_return(&c.header, Value::Null)
            .header
            .confidential
    );
    // The error path stays deliverable: it carries no value, and a
    // confidential error to an unattested caller would swallow the reason
    // the call failed.
    assert!(
        !Message::error_reply(&c.header, &Error::failed("no"))
            .header
            .confidential
    );
}

#[test]
fn an_ordinary_message_does_not_pay_for_private_flags() {
    let json = serde_json::to_string(&call()).unwrap();
    assert!(!json.contains("confidential"), "{json}");
    assert!(!json.contains("sensitive"), "{json}");
    assert!(!json.contains("stream_reply"), "{json}");
}

#[test]
fn a_header_from_a_peer_that_predates_the_flag_reads_as_not_confidential() {
    // Adding an optional field is a compatible change only if the old wire
    // form still parses. This is that guarantee, asserted rather than
    // assumed.
    let old = serde_json::json!({
        "kind": "method_call",
        "serial": 1,
        "destination": "ai.tinyhumans.openhuman.Voice",
        "path": "/ai/tinyhumans/openhuman/Voice",
        "interface": "ai.tinyhumans.openhuman.Voice",
        "member": "Transcribe"
    });
    let header: Header = serde_json::from_value(old).unwrap();
    assert!(!header.confidential);
    assert!(!header.sensitive);
    assert!(!header.stream_reply);
}

#[test]
fn messages_round_trip_through_json() {
    let c = call();
    let bytes = serde_json::to_vec(&c).unwrap();
    assert_eq!(serde_json::from_slice::<Message>(&bytes).unwrap(), c);
}
