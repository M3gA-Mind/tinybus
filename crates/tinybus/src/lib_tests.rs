use super::*;

#[test]
fn version_is_the_package_version() {
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
    assert!(!VERSION.is_empty());
}

#[test]
fn bus_constants_parse_as_the_names_they_claim_to_be() {
    BusName::try_from(BUS_NAME).expect("bus name");
    ObjectPath::try_from(BUS_PATH).expect("bus path");
    InterfaceName::try_from(BUS_INTERFACE).expect("bus interface");
}
