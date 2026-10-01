use super::*;

fn iface(name: &str) -> InterfaceName {
    InterfaceName::new(name).unwrap()
}

#[test]
fn versions_parse_and_round_trip() {
    let v = Version::parse("2.3.1").unwrap();
    assert_eq!((v.major, v.minor, v.patch), (2, 3, 1));
    assert_eq!(v.to_string(), "2.3.1");
    assert_eq!(
        Version::parse("1.0.0-rc.1").unwrap().tag.as_deref(),
        Some("rc.1")
    );
    assert_eq!(
        Version::parse("1.0.0-rc.1").unwrap().to_string(),
        "1.0.0-rc.1"
    );
}

#[test]
fn malformed_versions_are_refused() {
    assert!(Version::parse("1.2").is_err());
    assert!(Version::parse("1.2.3.4").is_err());
    assert!(Version::parse("1.2.x").is_err());
    assert!(Version::parse("").is_err());
}

#[test]
fn ordering_ignores_the_tag() {
    // A pre-release ordering nobody agreed on is worse than none: a peer
    // that needs to distinguish them is describing two interfaces.
    assert_eq!(
        Version::parse("1.0.0-rc1")
            .unwrap()
            .cmp(&Version::parse("1.0.0").unwrap()),
        Ordering::Equal
    );
    assert!(Version::parse("1.2.0").unwrap() < Version::parse("1.10.0").unwrap());
    assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.99.99").unwrap());
}

#[test]
fn a_provider_accepts_its_whole_series_but_a_consumer_only_looks_forward() {
    // The asymmetry, asserted directly: it is the single easiest thing to
    // get wrong here, and getting it wrong rejects every older client.
    let provider = Version::new(2, 3, 0).compatible_series();
    assert!(
        provider.accepts(&Version::new(2, 0, 0)),
        "an older caller still fits"
    );
    assert!(provider.accepts(&Version::new(2, 3, 0)));
    assert!(!provider.accepts(&Version::new(3, 0, 0)));

    let consumer = Version::new(2, 3, 0).caret();
    assert!(
        !consumer.accepts(&Version::new(2, 0, 0)),
        "an older provider does not"
    );
    assert!(consumer.accepts(&Version::new(2, 9, 0)));
}

#[test]
fn the_caret_rule_stops_at_the_next_major() {
    let range = Version::new(1, 2, 3).caret();
    assert!(range.accepts(&Version::new(1, 2, 3)));
    assert!(range.accepts(&Version::new(1, 9, 0)));
    assert!(!range.accepts(&Version::new(2, 0, 0)));
    // ...and never below the declared version, because a 1.2.0 client may
    // be using something 1.1.0 does not have.
    assert!(!range.accepts(&Version::new(1, 2, 2)));
}

#[test]
fn zero_x_treats_every_minor_as_breaking() {
    let range = Version::new(0, 3, 1).caret();
    assert!(range.accepts(&Version::new(0, 3, 9)));
    assert!(!range.accepts(&Version::new(0, 4, 0)));
}

#[test]
fn ranges_parse_in_all_three_spellings() {
    let explicit = VersionRange::parse(">=1.2.0, <2.0.0").unwrap();
    assert_eq!(explicit, VersionRange::parse("^1.2.0").unwrap());
    assert_eq!(explicit, VersionRange::parse("1.2.0").unwrap());
    assert_eq!(explicit.to_string(), ">=1.2.0, <2.0.0");

    let open = VersionRange::parse(">=1.0.0").unwrap();
    assert!(open.accepts(&Version::new(99, 0, 0)));
    assert!(
        VersionRange::parse("<2.0.0").is_err(),
        "a range needs a lower bound"
    );
    assert!(VersionRange::parse("~1.0.0").is_err());
}

fn provider(version: &str) -> PeerManifest {
    PeerManifest::new("voice-service").provides(InterfaceVersion::provided(
        iface("ai.tinyhumans.openhuman.Voice"),
        Version::parse(version).unwrap(),
    ))
}

fn consumer(version: &str) -> PeerManifest {
    PeerManifest::new("openhuman").consumes(InterfaceVersion::consumed(
        iface("ai.tinyhumans.openhuman.Voice"),
        Version::parse(version).unwrap(),
    ))
}

#[test]
fn a_newer_compatible_provider_is_accepted() {
    let result = check(
        &provider("2.3.0"),
        &consumer("2.1.0"),
        &iface("ai.tinyhumans.openhuman.Voice"),
    );
    assert!(result.is_compatible(), "{result}");
}

#[test]
fn a_major_bump_in_either_direction_is_caught() {
    let interface = iface("ai.tinyhumans.openhuman.Voice");

    // Provider ran ahead of the caller.
    let ahead = check(&provider("3.0.0"), &consumer("2.1.0"), &interface);
    assert!(!ahead.is_compatible(), "{ahead}");
    assert!(ahead.to_string().contains("does not accept"), "{ahead}");

    // Caller ran ahead of the provider. Both are caught by the consumer's
    // own range — it demands >=3.0.0 and is offered 2.1.0 — so the verdict
    // names the caller's requirement rather than the provider's, which is
    // the side that has to change.
    let behind = check(&provider("2.1.0"), &consumer("3.0.0"), &interface);
    assert!(
        matches!(behind, Compatibility::ConsumerRejects { .. }),
        "{behind}"
    );
    assert!(behind.to_string().contains("3.0.0"), "{behind}");
}

#[test]
fn an_older_provider_than_the_caller_needs_is_rejected() {
    // Same major, but the caller was written against 2.5 and the provider
    // only implements 2.1 — the caller may use something 2.1 lacks.
    let result = check(
        &provider("2.1.0"),
        &consumer("2.5.0"),
        &iface("ai.tinyhumans.openhuman.Voice"),
    );
    assert!(
        matches!(result, Compatibility::ConsumerRejects { .. }),
        "{result}"
    );
}

#[test]
fn a_provider_that_dropped_old_callers_says_so() {
    // The case only the provider-side check catches: the caller is happy
    // with what is offered, but the provider has narrowed its own support
    // window and will not serve a caller this old.
    let interface = iface("ai.tinyhumans.openhuman.Voice");
    let provider = PeerManifest::new("voice").provides(
        InterfaceVersion::provided(interface.clone(), Version::new(2, 3, 0))
            .with_accepts(VersionRange::parse(">=2.2.0, <3.0.0").unwrap()),
    );
    let consumer = PeerManifest::new("openhuman").consumes(InterfaceVersion::consumed(
        interface.clone(),
        Version::new(2, 0, 0),
    ));

    let result = check(&provider, &consumer, &interface);
    assert!(
        matches!(result, Compatibility::ProviderRejects { .. }),
        "{result}"
    );
    assert!(result.to_string().contains(">=2.2.0"), "{result}");
}

#[test]
fn an_undeclared_interface_is_reported_as_not_provided() {
    let result = check(
        &PeerManifest::new("voice-service"),
        &consumer("2.0.0"),
        &iface("ai.tinyhumans.openhuman.Voice"),
    );
    assert_eq!(result, Compatibility::NotProvided);
}

#[test]
fn a_consumer_that_declares_nothing_accepts_anything() {
    // Manifests roll out incrementally: a peer that has not adopted them
    // must not be locked off the bus by peers that have.
    let result = check(
        &provider("9.9.9"),
        &PeerManifest::new("legacy"),
        &iface("ai.tinyhumans.openhuman.Voice"),
    );
    assert!(result.is_compatible(), "{result}");
}

#[test]
fn an_explicit_range_can_widen_beyond_the_caret_default() {
    // A provider that genuinely kept 1.x compatibility across a major bump
    // can say so, rather than being forced into a rename.
    let provider = PeerManifest::new("voice").provides(
        InterfaceVersion::provided(
            iface("ai.tinyhumans.openhuman.Voice"),
            Version::new(2, 0, 0),
        )
        .with_accepts(VersionRange::parse(">=1.0.0, <3.0.0").unwrap()),
    );
    let consumer = PeerManifest::new("openhuman").consumes(
        InterfaceVersion::consumed(
            iface("ai.tinyhumans.openhuman.Voice"),
            Version::new(1, 5, 0),
        )
        .with_accepts(VersionRange::parse(">=1.0.0, <3.0.0").unwrap()),
    );
    let result = check(
        &provider,
        &consumer,
        &iface("ai.tinyhumans.openhuman.Voice"),
    );
    assert!(result.is_compatible(), "{result}");
}

#[test]
fn a_manifest_round_trips_through_json() {
    let manifest = PeerManifest::new("voice-service")
        .version(Version::new(0, 4, 2))
        .provides(InterfaceVersion::provided(
            iface("ai.tinyhumans.openhuman.Voice"),
            Version::new(2, 3, 0),
        ));
    let json = serde_json::to_string(&manifest).unwrap();
    assert_eq!(
        serde_json::from_str::<PeerManifest>(&json).unwrap(),
        manifest
    );
    // Versions travel as strings, so a manifest is readable in `monitor`.
    assert!(json.contains("\"2.3.0\""), "{json}");
}
