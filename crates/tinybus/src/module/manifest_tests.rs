use super::*;

fn manifest() -> ModuleManifest {
    ModuleManifest {
        schema: MANIFEST_SCHEMA,
        module: ModuleIdentity {
            name: "example".to_string(),
            version: Version::parse("1.2.3").unwrap(),
            description: String::new(),
            homepage: None,
            license: String::new(),
        },
        bus_name: BusName::new("ai.tinyhumans.Example").unwrap(),
        object_path: ObjectPath::new("/ai/tinyhumans/Example").unwrap(),
        provides: vec![ProvidedInterface {
            version: InterfaceVersion::provided(
                InterfaceName::new("ai.tinyhumans.Example").unwrap(),
                Version::parse("1.2.3").unwrap(),
            ),
            methods: vec![MemberName::new("Ping").unwrap()],
            signals: vec![MemberName::new("Changed").unwrap()],
        }],
        requires: vec![Dependency {
            interface: InterfaceVersion::consumed(
                InterfaceName::new("ai.tinyhumans.Dependency").unwrap(),
                Version::parse("1.0.0").unwrap(),
            ),
            optional: false,
            reason: String::new(),
        }],
        environment: vec![EnvSpec {
            name: "TOKEN".to_string(),
            required: true,
            description: String::new(),
        }],
        capabilities: vec![Capability {
            name: "network".to_string(),
            description: String::new(),
        }],
        lazy_init: false,
        worker_threads: 1,
        on_panic: PanicPolicy::Detach,
    }
}

#[test]
fn a_manifest_round_trips_and_omits_empty_optional_metadata() {
    let value = manifest();
    let json = serde_json::to_value(&value).unwrap();
    assert!(json["module"].get("description").is_none());
    assert!(json["module"].get("homepage").is_none());
    assert_eq!(
        serde_json::from_value::<ModuleManifest>(json).unwrap(),
        value
    );
}

#[test]
fn defaults_make_a_minimal_manifest_safe_to_load() {
    let manifest: ModuleManifest = serde_json::from_value(serde_json::json!({
        "tinybus_manifest": MANIFEST_SCHEMA,
        "module": { "name": "example", "version": "1.0.0" },
        "bus_name": "ai.tinyhumans.Example",
        "object_path": "/ai/tinyhumans/Example"
    }))
    .unwrap();
    assert!(manifest.provides.is_empty());
    assert!(manifest.requires.is_empty());
    assert_eq!(manifest.worker_threads, 1);
    assert_eq!(manifest.on_panic, PanicPolicy::Detach);
}

#[test]
fn peer_manifest_and_interface_lookup_follow_the_declaration() {
    let manifest = manifest();
    assert_eq!(manifest.peer_manifest().name, "example");
    assert!(
        manifest
            .provided(&InterfaceName::new("ai.tinyhumans.Example").unwrap())
            .is_some()
    );
    assert!(
        manifest
            .provided(&InterfaceName::new("ai.tinyhumans.Missing").unwrap())
            .is_none()
    );
}
