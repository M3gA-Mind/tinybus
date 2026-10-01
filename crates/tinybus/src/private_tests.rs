use super::*;

#[test]
fn generated_helpers_round_trip_values_and_redact_bad_arguments() {
    let member = MemberName::new("Add").unwrap();
    let decoded: (u32, u32) = decode_args(&member, serde_json::json!([2, 3])).unwrap();
    assert_eq!(decoded, (2, 3));
    assert_eq!(encode_reply(&5u32).unwrap(), serde_json::json!(5));

    let sensitive = "sensitive-argument-value";
    let error = decode_args::<(u32,)>(&member, serde_json::json!([sensitive])).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("Add"));
    assert!(rendered.contains("bad arguments"));
    assert!(!rendered.contains(sensitive));
}

#[test]
fn literal_parsers_reject_invalid_generated_names() {
    assert_eq!(
        parse_interface("ai.tinyhumans.Example").as_str(),
        "ai.tinyhumans.Example"
    );
    assert_eq!(parse_member("Call").as_str(), "Call");
    assert!(std::panic::catch_unwind(|| parse_interface("not-valid")).is_err());
    assert!(std::panic::catch_unwind(|| parse_member("not.valid")).is_err());
}

#[test]
fn unknown_method_uses_a_safe_fallback_member() {
    let error = unknown_method("ai.tinyhumans.Example", "not.valid");
    let rendered = error.to_string();
    assert!(rendered.contains("ai.tinyhumans.Example"));
    assert!(rendered.contains("Unknown"));
}
