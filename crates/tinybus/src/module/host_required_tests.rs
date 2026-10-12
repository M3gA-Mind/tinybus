//! Required manifest admission must precede the setup callback.
use super::*;
use crate::module::{ModuleRequirements, RequiredInterface};
use crate::name::{InterfaceName, MemberName};
use crate::version::VersionRange;
use std::sync::atomic::{AtomicUsize, Ordering};

static SETUPS: AtomicUsize = AtomicUsize::new(0);
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

unsafe extern "C" fn counted_setup(
    _: *const crate::module::abi::TbHostVtable,
    _: *mut TbModuleVtable,
) -> i32 {
    SETUPS.fetch_add(1, Ordering::SeqCst);
    crate::module::abi::TB_BAD_ARGUMENT
}

fn required() -> ModuleRequirements {
    ModuleRequirements::new(
        "clock",
        BusName::new("ai.tinyhumans.module.Clock").unwrap(),
        ObjectPath::new("/ai/tinyhumans/module/Clock").unwrap(),
        vec![
            RequiredInterface::new(
                InterfaceName::new("ai.tinyhumans.module.Clock").unwrap(),
                VersionRange::new(Version::new(1, 15, 0), Some(Version::new(2, 0, 0))),
                vec![MemberName::new("RoutedRead").unwrap()],
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

#[test]
fn an_unsupported_actual_manifest_is_refused_before_setup() {
    let _lock = LOCK.lock().unwrap();
    SETUPS.store(0, Ordering::SeqCst);
    let host = ModuleHost::new(Broker::new());
    let artifact = LoadedArtifact {
        descriptor: TbAbiDescriptor::current("clock", "0.1.0"),
        manifest: super::tests::manifest(),
        init: counted_setup,
    };
    assert!(
        host.activate_required(
            Path::new("clock.so"),
            artifact,
            serde_json::json!({}),
            &required()
        )
        .is_err()
    );
    assert_eq!(
        SETUPS.load(Ordering::SeqCst),
        0,
        "incompatible setup must never run"
    );
}

#[tokio::test]
async fn a_provisional_lazy_sidecar_cannot_satisfy_activation_requirements() {
    let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let filename = if cfg!(windows) {
        "clock.dll"
    } else if cfg!(target_os = "macos") {
        "clock.dylib"
    } else {
        "clock.so"
    };
    let path = directory.path().join(filename);
    std::fs::write(&path, b"not an actual module").unwrap();
    let mut manifest = super::tests::manifest();
    manifest.lazy_init = true;
    std::fs::write(
        path.with_file_name(format!("{filename}.manifest.json")),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let host = ModuleHost::new(Broker::new());
    assert!(
        host.load_file_with_requirements(&path, serde_json::json!({}), &required())
            .is_err(),
        "lazy metadata cannot authorize initialization"
    );
    assert!(
        host.list().is_empty(),
        "a refused artifact must not register for later calls"
    );
}
