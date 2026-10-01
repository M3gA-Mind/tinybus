use super::*;
use crate::message::Message;
use crate::name::{BusName, InterfaceName, MemberName, ObjectPath};

fn sample() -> Message {
    Message::method_call(
        BusName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        ObjectPath::new("/ai/tinyhumans/openhuman/Voice").unwrap(),
        InterfaceName::new("ai.tinyhumans.openhuman.Voice").unwrap(),
        MemberName::new("Transcribe").unwrap(),
        // A body containing the delimiter a newline-framed codec would
        // have choked on.
        serde_json::json!(["line one\nline two"]),
    )
}

#[test]
fn a_frame_round_trips_with_its_newlines_intact() {
    let frame = encode(&sample()).unwrap();
    let len = decode_length(frame[..4].try_into().unwrap()).unwrap();
    assert_eq!(len, frame.len() - LENGTH_PREFIX_LEN);
    let decoded: Message = decode(&frame[LENGTH_PREFIX_LEN..]).unwrap();
    assert_eq!(decoded, sample());
}

#[test]
fn an_announced_oversize_frame_is_rejected_before_allocating() {
    let err = decode_length(u32::MAX.to_be_bytes()).unwrap_err();
    assert!(err.to_string().contains("over the"), "{err}");
}

#[test]
fn an_oversize_payload_is_refused_at_encode_time() {
    let huge = "x".repeat(MAX_FRAME_LEN + 1);
    let err = encode(&serde_json::json!(huge)).unwrap_err();
    assert!(err.to_string().contains("exceeds the"), "{err}");
}

#[test]
fn a_value_that_cannot_be_serialized_is_an_encode_error_not_a_panic() {
    // JSON object keys must be strings; a map keyed by a tuple cannot encode.
    let mut unencodable = std::collections::BTreeMap::new();
    unencodable.insert((1, 2), "value");
    assert!(encode(&unencodable).is_err());
}

#[test]
fn a_malformed_payload_is_a_decode_error_not_a_panic() {
    let err = decode::<Message>(b"{not json").unwrap_err();
    assert!(!err.to_string().is_empty());
}
