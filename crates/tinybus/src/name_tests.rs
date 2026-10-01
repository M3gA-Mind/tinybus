use super::*;

#[test]
fn well_known_and_unique_names_both_parse() {
    assert!(
        !BusName::new("ai.tinyhumans.openhuman.Voice")
            .unwrap()
            .is_unique()
    );
    assert!(BusName::new(":1.42").unwrap().is_unique());
}

#[test]
fn every_name_grammar_rejects_its_boundary_cases() {
    let overlong = "a".repeat(MAX_NAME_LEN + 1);
    assert!(InterfaceName::new("").is_err());
    assert!(InterfaceName::new(overlong.clone()).is_err());
    assert!(InterfaceName::new("ai.tinyhumans.bad!").is_err());
    assert!(ObjectPath::new(overlong.clone()).is_err());
    assert!(MemberName::new("").is_err());
    assert!(MemberName::new(overlong).is_err());
    assert!(MemberName::new("1Call").is_err());
}

#[test]
fn generated_conversions_preserve_each_valid_name() {
    let bus: BusName = "ai.tinyhumans.Example".parse().unwrap();
    let path = ObjectPath::try_from("/ai/tinyhumans/Example").unwrap();
    let interface = InterfaceName::try_from("ai.tinyhumans.Example".to_string()).unwrap();
    let member = MemberName::new("Call").unwrap();
    assert_eq!(bus.to_string(), bus.as_ref());
    assert_eq!(String::from(path), "/ai/tinyhumans/Example");
    assert_eq!(interface.as_ref(), "ai.tinyhumans.Example");
    assert_eq!(member.as_ref(), "Call");
}

#[test]
fn a_single_element_is_not_a_bus_name() {
    // The two-element minimum is what stops an integration from squatting
    // `Voice` and colliding with every other vendor on the bus.
    let err = BusName::new("Voice").unwrap_err();
    assert!(
        err.to_string().contains("two dot-separated elements"),
        "{err}"
    );
}

#[test]
fn a_unique_name_must_be_digits() {
    assert!(BusName::new(":1.beta").is_err());
    assert!(BusName::new(":").is_err());
}

#[test]
fn dotted_names_reject_empty_and_leading_digit_elements() {
    assert!(InterfaceName::new("ai..Voice").is_err());
    assert!(InterfaceName::new("ai.9lives").is_err());
    assert!(InterfaceName::new("ai.tiny-humans.Voice").is_ok());
}

#[test]
fn over_long_names_are_refused_rather_than_stored() {
    let long = format!("ai.{}", "x".repeat(MAX_NAME_LEN));
    assert!(InterfaceName::new(long).is_err());
}

#[test]
fn object_paths_follow_the_slash_grammar() {
    assert!(ObjectPath::new("/").is_ok());
    assert!(ObjectPath::new("/ai/tinyhumans/openhuman/Voice").is_ok());
    assert!(ObjectPath::new("ai/tinyhumans").is_err());
    assert!(ObjectPath::new("/ai/").is_err());
    assert!(ObjectPath::new("/ai//voice").is_err());
    assert!(ObjectPath::new("/ai/voice-1").is_err());
}

#[test]
fn subtree_matching_does_not_match_a_sibling_with_a_shared_prefix() {
    let mail = ObjectPath::new("/ai/Mail").unwrap();
    let account = ObjectPath::new("/ai/Mail/work").unwrap();
    let sibling = ObjectPath::new("/ai/Mailbox").unwrap();
    assert!(account.starts_with(&mail));
    assert!(mail.starts_with(&mail));
    assert!(!sibling.starts_with(&mail));
    assert!(sibling.starts_with(&ObjectPath::root()));
}

#[test]
fn names_round_trip_through_json() {
    let name = BusName::new("ai.tinyhumans.openhuman.Voice").unwrap();
    let json = serde_json::to_string(&name).unwrap();
    assert_eq!(json, "\"ai.tinyhumans.openhuman.Voice\"");
    assert_eq!(serde_json::from_str::<BusName>(&json).unwrap(), name);
}

#[test]
fn deserializing_a_malformed_name_fails_rather_than_producing_one() {
    // The router's invariant — every stored name is valid — depends on
    // this: the wire is the only place an unvalidated name can enter.
    assert!(serde_json::from_str::<BusName>("\"nope\"").is_err());
}

#[test]
fn member_names_reject_dots_so_match_rules_stay_unambiguous() {
    assert!(MemberName::new("Transcribe").is_ok());
    assert!(MemberName::new("Voice.Transcribe").is_err());
}
