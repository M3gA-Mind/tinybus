use super::*;

#[test]
fn every_known_feature_bit_has_a_stable_name() {
    assert_eq!(feature_name(FEATURE_UDS), "uds");
    assert_eq!(feature_name(FEATURE_MACROS), "macros");
    assert_eq!(feature_name(FEATURE_CLI), "cli");
    assert_eq!(feature_name(FEATURE_MODULES), "modules");
}

#[test]
fn unknown_feature_bits_do_not_leak_an_untrusted_value() {
    assert_eq!(feature_name(1 << 63), "unknown");
}
