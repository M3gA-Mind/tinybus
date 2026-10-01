use super::*;
use crate::module::manifest::{
    Dependency, MANIFEST_SCHEMA, ModuleIdentity, PanicPolicy, ProvidedInterface,
};
use crate::{BusName, InterfaceName, InterfaceVersion, ObjectPath, Version};

fn module(name: &str, provides: &[&str], requires: &[(&str, bool)]) -> ModuleManifest {
    let version = Version::new(1, 0, 0);
    ModuleManifest {
        schema: MANIFEST_SCHEMA,
        module: ModuleIdentity {
            name: name.to_string(),
            version: version.clone(),
            description: String::new(),
            homepage: None,
            license: String::new(),
        },
        bus_name: BusName::new(format!("ai.tinyhumans.module.{name}")).unwrap(),
        object_path: ObjectPath::new(format!("/ai/tinyhumans/module/{name}")).unwrap(),
        provides: provides
            .iter()
            .map(|name| ProvidedInterface {
                version: InterfaceVersion::provided(
                    InterfaceName::new(*name).unwrap(),
                    version.clone(),
                ),
                methods: Vec::new(),
                signals: Vec::new(),
            })
            .collect(),
        requires: requires
            .iter()
            .map(|(name, optional)| Dependency {
                interface: InterfaceVersion::consumed(
                    InterfaceName::new(*name).unwrap(),
                    version.clone(),
                ),
                optional: *optional,
                reason: String::new(),
            })
            .collect(),
        environment: Vec::new(),
        capabilities: Vec::new(),
        lazy_init: false,
        worker_threads: 1,
        on_panic: PanicPolicy::Detach,
    }
}

#[test]
fn a_required_dependency_no_module_provides_leaves_the_module_unresolved_and_names_the_interface() {
    let manifests = [module(
        "Consumer",
        &[],
        &[("ai.tinyhumans.module.Missing", false)],
    )];
    let result = resolve(&manifests, &HashSet::new());
    assert!(result.order.is_empty());
    assert!(
        result.unresolved[0]
            .1
            .contains("ai.tinyhumans.module.Missing")
    );
}

#[test]
fn an_optional_dependency_that_is_missing_still_resolves() {
    let manifests = [module(
        "Consumer",
        &[],
        &[("ai.tinyhumans.module.Missing", true)],
    )];
    assert_eq!(resolve(&manifests, &HashSet::new()).order, [0]);
}

#[test]
fn a_dependency_cycle_is_reported_rather_than_loaded() {
    let manifests = [
        module(
            "One",
            &["ai.tinyhumans.module.One"],
            &[("ai.tinyhumans.module.Two", false)],
        ),
        module(
            "Two",
            &["ai.tinyhumans.module.Two"],
            &[("ai.tinyhumans.module.One", false)],
        ),
    ];
    let result = resolve(&manifests, &HashSet::new());
    assert!(result.order.is_empty());
    assert!(
        result
            .unresolved
            .iter()
            .all(|(_, reason)| reason == "module dependency cycle detected")
    );
}

#[test]
fn a_module_blocked_behind_an_unresolved_provider_is_not_reported_as_a_cycle() {
    let manifests = [
        module("Consumer", &[], &[("ai.tinyhumans.module.Provider", false)]),
        module(
            "Provider",
            &["ai.tinyhumans.module.Provider"],
            &[("ai.tinyhumans.module.Missing", false)],
        ),
    ];
    let result = resolve(&manifests, &HashSet::new());
    assert!(result.order.is_empty());
    assert!(
        result
            .unresolved
            .iter()
            .all(|(_, reason)| reason.contains("has no provider"))
    );
}

#[test]
fn a_module_is_initialized_after_every_module_it_depends_on() {
    let manifests = [
        module("Consumer", &[], &[("ai.tinyhumans.module.Provider", false)]),
        module("Provider", &["ai.tinyhumans.module.Provider"], &[]),
    ];
    assert_eq!(resolve(&manifests, &HashSet::new()).order, [1, 0]);
}

#[test]
fn two_modules_claiming_one_bus_name_are_both_reported_and_neither_is_initialized() {
    let first = module("One", &[], &[]);
    let mut second = module("Two", &[], &[]);
    second.bus_name = first.bus_name.clone();
    let result = resolve(&[first, second], &HashSet::new());
    assert!(result.order.is_empty());
    assert_eq!(result.unresolved.len(), 2);
}
