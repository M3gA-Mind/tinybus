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

mod resolver {
    use super::super::*;
    use crate::module::failure::{FailureReport, FailureSink};
    use crate::module::runtime::RuntimeNames;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Recording(Mutex<Vec<(String, Reason)>>);

    impl FailureSink for Recording {
        fn capture(&self, report: &FailureReport<'_>) {
            self.0
                .lock()
                .unwrap()
                .push((report.module.to_string(), report.reason));
        }
    }

    fn shared_runtime() -> &'static ModuleRuntime {
        static RUNTIME: std::sync::OnceLock<ModuleRuntime> = std::sync::OnceLock::new();
        RUNTIME.get_or_init(|| ModuleRuntime::start(&RuntimeNames::default(), None).unwrap())
    }

    /// The resolution table is process-global, so each test owns a distinct id.
    fn record(id: &'static str) -> &'static ReleaseRecord {
        Box::leak(Box::new(ReleaseRecord {
            id,
            bus_name: Box::leak(format!("ai.tinyhumans.test.{id}").into_boxed_str()),
            object_path: "/ai/tinyhumans/test/Module",
            version: "0.0.0",
            release_url: "https://github.com/tinyhumansai/tinybus/releases/tag/v0.0.0",
            assets: &[],
        }))
    }

    fn admitted_info(record: &ReleaseRecord, state: ModuleState) -> ModuleInfo {
        let mut manifest = super::module(record.id, &[], &[]);
        manifest.bus_name = crate::BusName::new(record.bus_name).unwrap();
        manifest.object_path = crate::ObjectPath::new(record.object_path).unwrap();
        ModuleInfo {
            name: manifest.module.name.clone(),
            version: manifest.module.version.to_string(),
            file: "module.dylib".into(),
            state,
            manifest,
            rustc_version: String::new(),
            rustc_mismatch: false,
            enabled: true,
        }
    }

    unsafe extern "C" fn accept_raced_message(
        _: *mut std::ffi::c_void,
        _: *const u8,
        _: usize,
    ) -> i32 {
        crate::module::abi::TB_OK
    }

    unsafe extern "C" fn stop_raced_module(ctx: *mut std::ffi::c_void, _: u64) -> i32 {
        if !ctx.is_null() {
            drop(unsafe { Box::from_raw(ctx.cast::<()>()) });
        }
        crate::module::abi::TB_OK
    }

    unsafe extern "C" fn initialize_raced_module(
        host: *const crate::module::abi::TbHostVtable,
        module: *mut crate::module::abi::TbModuleVtable,
    ) -> i32 {
        let host = unsafe { *host };
        unsafe {
            *module = crate::module::abi::TbModuleVtable {
                size: crate::module::abi::TB_MODULE_VTABLE_BASE_SIZE,
                _reserved: 0,
                module_ctx: Box::into_raw(Box::new(())).cast(),
                deliver: accept_raced_message,
                shutdown: stop_raced_module,
                reinitialize: None,
            }
        };
        unsafe { (host.ready)(host.host_ctx) };
        crate::module::abi::TB_OK
    }

    fn raced_module() -> crate::Result<LinkedModule> {
        let mut manifest = super::module("resolver_raced_module", &[], &[]);
        manifest.bus_name = crate::BusName::new("ai.tinyhumans.test.resolver_raced_module")?;
        manifest.object_path = crate::ObjectPath::new("/ai/tinyhumans/test/Module")?;
        Ok(LinkedModule {
            descriptor: crate::module::abi::TbAbiDescriptor::current(
                "resolver_raced_module",
                "1.0.0",
            ),
            manifest,
            init: initialize_raced_module,
        })
    }

    fn raced_search_module() -> crate::Result<LinkedModule> {
        let mut manifest = super::module("resolver_search_raced_module", &[], &[]);
        manifest.bus_name = crate::BusName::new("ai.tinyhumans.test.resolver_search_raced_module")?;
        manifest.object_path = crate::ObjectPath::new("/ai/tinyhumans/test/Module")?;
        Ok(LinkedModule {
            descriptor: crate::module::abi::TbAbiDescriptor::current(
                "resolver_search_raced_module",
                "1.0.0",
            ),
            manifest,
            init: initialize_raced_module,
        })
    }

    fn raced_cache_module() -> crate::Result<LinkedModule> {
        let mut manifest = super::module("resolver_cache_raced_module", &[], &[]);
        manifest.bus_name = crate::BusName::new("ai.tinyhumans.test.resolver_cache_raced_module")?;
        manifest.object_path = crate::ObjectPath::new("/ai/tinyhumans/test/Module")?;
        Ok(LinkedModule {
            descriptor: crate::module::abi::TbAbiDescriptor::current(
                "resolver_cache_raced_module",
                "1.0.0",
            ),
            manifest,
            init: initialize_raced_module,
        })
    }

    #[test]
    fn a_duplicate_admission_error_recovers_when_the_requested_module_is_initializing() {
        let record = record("resolver_raced_admission");
        // This snapshot is taken after a competing host path won admission but
        // before this resolver's load attempt returned its duplicate-name error.
        let modules = [admitted_info(record, ModuleState::Initializing)];
        let result = recover_admission(
            Err("module name is already loaded".into()),
            &modules,
            record,
        );
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn ensure_loaded_recovers_when_direct_host_admission_wins_after_its_initial_check() {
        let record = record("resolver_raced_module");
        let sources = HashMap::from([(record.id.to_string(), ModuleSource::Linked)]);
        let mut f = fixture(record, true, sources.clone(), None);
        let runtime = shared_runtime();
        f.resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources,
            linked: HashMap::from([(record.id.to_string(), raced_module as LinkedFactory)]),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: None,
            records: vec![record],
            terminal_marker: "This is terminal for the running process".into(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(f.sink.clone())),
        });
        f.resolver.after_initial_host_check = Some(Arc::new(|runtime, _| {
            runtime
                .host()
                .attach_linked_with_config(raced_module().unwrap(), serde_json::json!({}))
                .unwrap();
        }));

        assert_eq!(f.resolver.ensure_loaded(record.id).await, Ok(()));
        assert!(runtime.host().list().iter().any(|info| {
            info.manifest.bus_name.as_str() == record.bus_name
                && info.manifest.object_path.as_str() == record.object_path
        }));
        assert_eq!(f.resolver.state_of(record.id), Availability::Ready);
    }

    #[tokio::test]
    async fn ensure_loaded_rechecks_host_after_search_before_touching_the_release_cache() {
        let record = record("resolver_search_raced_module");
        let dir = tempfile::tempdir().unwrap();
        let mut f = fixture(record, true, HashMap::new(), Some(dir.path().to_path_buf()));
        let runtime = shared_runtime();
        f.resolver.after_initial_host_check = Some(Arc::new(|runtime, _| {
            runtime
                .host()
                .attach_linked_with_config(raced_search_module().unwrap(), serde_json::json!({}))
                .unwrap();
        }));

        assert_eq!(f.resolver.ensure_loaded(record.id).await, Ok(()));
        assert!(runtime.host().list().iter().any(|info| {
            info.manifest.bus_name.as_str() == record.bus_name
                && info.manifest.object_path.as_str() == record.object_path
        }));
        assert_eq!(
            f.config_calls.load(Ordering::SeqCst),
            0,
            "the release-cache rung ran after the module was admitted"
        );
    }

    #[tokio::test]
    async fn recovering_after_a_cache_error_does_not_prune_other_cached_versions() {
        let record = record("resolver_cache_raced_module");
        let install_dir = tempfile::tempdir().unwrap();
        let stale = install_dir.path().join(record.id).join("old-version");
        std::fs::create_dir_all(&stale).unwrap();
        let marker = stale.join("preserve-me");
        std::fs::write(&marker, "cached by the other host load").unwrap();
        let sink = Arc::new(Recording::default());
        let runtime = shared_runtime();
        let resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: Some(install_dir.path().to_path_buf()),
            allow_download: false,
            bundled_dir: None,
            sources: HashMap::new(),
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: None,
            records: vec![record],
            terminal_marker: "This is terminal for the running process".into(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(move |_| {
                runtime
                    .host()
                    .attach_linked_with_config(raced_cache_module().unwrap(), serde_json::json!({}))
                    .unwrap();
                serde_json::json!({})
            }),
            reporter: Arc::new(FailureReporter::new(sink)),
        });

        assert_eq!(resolver.ensure_loaded(record.id).await, Ok(()));
        assert!(runtime.host().list().iter().any(|info| {
            info.manifest.bus_name.as_str() == record.bus_name
                && info.manifest.object_path.as_str() == record.object_path
        }));
        assert!(
            marker.exists(),
            "a recovered cache failure pruned its sibling cache"
        );
    }

    #[test]
    fn admission_recovery_requires_the_exact_enabled_nonterminal_module() {
        let record = record("resolver_raced_admission_guards");
        let error = "module name is already loaded".to_string();
        let wrong_path = {
            let mut info = admitted_info(record, ModuleState::Ready);
            info.manifest.object_path = crate::ObjectPath::new("/other/module").unwrap();
            info
        };
        let wrong_bus = {
            let mut info = admitted_info(record, ModuleState::Ready);
            info.manifest.bus_name = crate::BusName::new("ai.tinyhumans.module.Other").unwrap();
            info
        };
        let disabled = {
            let mut info = admitted_info(record, ModuleState::Ready);
            info.enabled = false;
            info
        };
        for info in [
            wrong_path,
            wrong_bus,
            disabled,
            admitted_info(
                record,
                ModuleState::Failed {
                    reason: "init failed".into(),
                },
            ),
            admitted_info(
                record,
                ModuleState::Faulted {
                    reason: "faulted".into(),
                },
            ),
            admitted_info(record, ModuleState::Stopped),
            admitted_info(record, ModuleState::Disabled),
        ] {
            assert_eq!(
                recover_admission(Err(error.clone()), &[info], record),
                Err(error.clone())
            );
        }
    }

    struct Fixture {
        sink: Arc<Recording>,
        config_calls: Arc<AtomicUsize>,
        runtime_calls: Arc<AtomicUsize>,
        resolver: Resolver,
    }

    fn fixture(
        record: &'static ReleaseRecord,
        enabled: bool,
        sources: HashMap<String, ModuleSource>,
        install_dir: Option<PathBuf>,
    ) -> Fixture {
        let sink = Arc::new(Recording::default());
        let config_calls = Arc::new(AtomicUsize::new(0));
        let runtime_calls = Arc::new(AtomicUsize::new(0));
        let counted = config_calls.clone();
        let started = runtime_calls.clone();
        let resolver = Resolver::new(ResolverInputs {
            enabled,
            install_dir,
            allow_download: false,
            bundled_dir: None,
            sources,
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: None,
            records: vec![record],
            terminal_marker: "This is terminal for the running process".to_string(),
            runtime: Arc::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(shared_runtime()) })
            }),
            module_config: Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                serde_json::json!({})
            }),
            reporter: Arc::new(FailureReporter::new(sink.clone())),
        });
        Fixture {
            sink,
            config_calls,
            runtime_calls,
            resolver,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires TINYBUS_TEST_HOST_CALLBACK_MODULE to point at the built cdylib"]
    async fn resolver_loads_a_verified_module_with_only_its_prepared_host_callback() {
        use crate::module::HostCallbackEndpoint;
        use crate::{Interface, InterfaceName, MemberName};
        use std::sync::atomic::AtomicUsize;

        struct Callback(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl Interface for Callback {
            fn name(&self) -> InterfaceName { "org.example.Callback".parse().unwrap() }
            fn members(&self) -> Vec<MemberName> { vec!["Observe".parse().unwrap()] }
            async fn call(&self, _: &MemberName, _: serde_json::Value) -> crate::Result<serde_json::Value> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!("org.example.Callback"))
            }
        }

        let source = PathBuf::from(std::env::var_os("TINYBUS_TEST_HOST_CALLBACK_MODULE").unwrap());
        let hash = crate::module::hash::file_hex(std::fs::File::open(&source).unwrap()).unwrap();
        let staging = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = staging.path().join(source.file_name().unwrap());
        std::fs::copy(&source, &path).unwrap();
        std::fs::write(staging.path().join("modules.toml"),
            format!("{:?} = {hash:?}\n", source.file_name().unwrap().to_str().unwrap())).unwrap();
        let record = Box::leak(Box::new(ReleaseRecord {
            id: "resolver-host-callback",
            bus_name: "org.example.EagerCallback",
            object_path: "/module",
            version: "0.0.0",
            release_url: "https://github.com/tinyhumansai/tinybus/releases/tag/v0.0.0",
            assets: &[],
        }));
        let runtime = shared_runtime();
        let binding = Arc::new(runtime.host().prepare_host_callbacks(vec![HostCallbackEndpoint {
            path: "/callback".parse().unwrap(),
            interface: "org.example.Callback".parse().unwrap(),
            members: vec!["Observe".parse().unwrap()],
        }]).await.unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        binding.serve_at("/callback".parse().unwrap(), Callback(calls.clone())).await.unwrap();
        let f = fixture(record, true, HashMap::from([(
            record.id.to_string(), ModuleSource::LocalFile { path: path.to_string_lossy().into_owned() }
        )]), None);

        let loaded = f.resolver.ensure_loaded_with_host_callbacks(record.id, binding.clone(),
            serde_json::json!({ "host": binding.unique_name() })).await;
        assert!(loaded.is_ok(), "{loaded:?}; callbacks={}", calls.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "resolver init did not use its exact prepared callback");
        assert!(runtime.host().list().iter().any(|module| module.manifest.bus_name.as_str() == "org.example.EagerCallback"));
    }

    #[tokio::test]
    async fn a_faulted_module_is_never_retried_in_process() {
        let record = record("resolver-faulted");
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(record, true, HashMap::new(), Some(dir.path().to_path_buf()));

        let first = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(first.contains("not available for this platform"), "{first}");
        assert!(first.contains("restart the app to try again"), "{first}");
        assert_eq!(f.resolver.state_of(record.id), Availability::Failed);
        assert_eq!(f.config_calls.load(Ordering::SeqCst), 1);

        // The cached outcome is returned verbatim, without another attempt.
        for _ in 0..3 {
            let again = f.resolver.ensure_loaded(record.id).await.unwrap_err();
            assert_eq!(again, first);
        }
        assert_eq!(
            f.config_calls.load(Ordering::SeqCst),
            1,
            "retried a faulted module"
        );
        // One terminal report, however many callers asked.
        assert_eq!(
            f.sink.0.lock().unwrap().as_slice(),
            &[(record.id.to_string(), Reason::ResolutionFailed)]
        );
    }

    #[tokio::test]
    async fn status_of_reports_the_failure_reason_and_unsupported_platforms() {
        let record = record("resolver-status");
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(record, true, HashMap::new(), Some(dir.path().to_path_buf()));

        // No asset is published for this host.
        assert_eq!(
            f.resolver.status_of(record),
            (
                Availability::Unsupported,
                Some("no artifact is published for this platform".to_string())
            )
        );
        let _ = f.resolver.ensure_loaded(record.id).await;
        let (state, detail) = f.resolver.status_of(record);
        assert_eq!(state, Availability::Failed);
        assert!(detail.unwrap().contains("restart the app"));
    }

    #[test]
    fn disabled_configuration_wins_over_a_loaded_module() {
        let record = record("resolver-disabled-status");
        let f = fixture(record, false, HashMap::new(), None);
        assert_eq!(
            f.resolver.status_of(record),
            (
                Availability::Unsupported,
                Some("modules are disabled in configuration".to_string())
            )
        );
    }

    #[tokio::test]
    async fn refusals_never_start_the_module_runtime() {
        let record = record("resolver-refusal");
        let f = fixture(record, false, HashMap::new(), None);
        let error = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(error.contains("modules are disabled"));
        let error = f.resolver.ensure_loaded("nobody").await.unwrap_err();
        // Disabled is checked before the registry, as it always was.
        assert!(error.contains("modules are disabled"));
        let enabled = fixture(record, true, HashMap::new(), None);
        let error = enabled.resolver.ensure_loaded("nobody").await.unwrap_err();
        assert_eq!(error, "unknown module 'nobody'");
        assert_eq!(enabled.runtime_calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.runtime_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.sink.0.lock().unwrap().as_slice(),
            &[(record.id.to_string(), Reason::Disabled)]
        );
    }

    #[tokio::test]
    async fn a_missing_install_dir_is_terminal() {
        let record = record("resolver-no-install-dir");
        let f = fixture(record, true, HashMap::new(), None);
        let error = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(error.contains("no directory is available to install modules into"));
    }

    #[tokio::test]
    async fn a_local_override_is_tried_before_the_release_cache() {
        let record = record("resolver-override");
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent-module");
        let sources = HashMap::from([(
            record.id.to_string(),
            ModuleSource::LocalFile {
                path: missing.to_string_lossy().into_owned(),
            },
        )]);
        // No install dir: reaching the cache rung would say so instead.
        let f = fixture(record, true, sources, None);
        let error = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(
            error.contains("could not be loaded from its local artifact"),
            "{error}"
        );
        assert!(!error.contains("no directory is available"), "{error}");
        assert_eq!(
            error
                .matches("This is terminal for the running process")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_bounded_wait_reports_still_loading_without_cancelling_the_resolution() {
        let record = record("resolver-bounded");
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(record, true, HashMap::new(), Some(dir.path().to_path_buf()));
        let table = resolution::global();
        let Claim::Run { sender, .. } = table.claim(record.id) else {
            panic!("first claim must run");
        };
        let waited = f
            .resolver
            .ensure_loaded_within(record.id, Some(Duration::from_millis(20)))
            .await;
        assert_eq!(waited, Err(LoadError::StillLoading));
        assert_eq!(f.resolver.state_of(record.id), Availability::Loading);
        table.complete(record.id, Resolution::Ready, sender);
        assert_eq!(f.resolver.ensure_loaded(record.id).await, Ok(()));
        assert_eq!(f.resolver.state_of(record.id), Availability::Ready);
    }

    #[tokio::test]
    async fn a_gated_module_is_refused_before_anything_starts_or_is_reported() {
        let record = record("resolver-gated");
        let mut f = fixture(record, true, HashMap::new(), None);
        let sink = Arc::new(Recording::default());
        let runtime_calls = Arc::new(AtomicUsize::new(0));
        let started = runtime_calls.clone();
        f.resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources: HashMap::new(),
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: Some(Arc::new(|id| {
                (id == "resolver-gated").then_some("test-switch")
            })),
            on_resolved: None,
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(shared_runtime()) })
            }),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(sink.clone())),
        });
        let refused = f
            .resolver
            .ensure_loaded_within(record.id, None)
            .await
            .unwrap_err();
        assert_eq!(refused, LoadError::Disabled("test-switch"));
        assert!(refused.into_message().contains("test-switch"));
        assert_eq!(runtime_calls.load(Ordering::SeqCst), 0);
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "a switch is not a failure"
        );
        assert_eq!(f.resolver.state_of(record.id), Availability::Available);
    }

    #[tokio::test]
    async fn a_linked_source_without_a_factory_is_terminal_and_never_retried() {
        let record = record("resolver-linked-missing");
        let sources = HashMap::from([(record.id.to_string(), ModuleSource::Linked)]);
        let f = fixture(record, true, sources, None);
        let first = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(first.contains("no linked factory is registered"), "{first}");
        assert!(!first.contains("no directory is available"), "{first}");
        assert_eq!(f.resolver.state_of(record.id), Availability::Failed);
        assert_eq!(
            f.resolver.ensure_loaded(record.id).await.unwrap_err(),
            first
        );
        assert_eq!(f.config_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failing_linked_factory_is_terminal_and_names_no_cause() {
        let record = record("resolver-linked-failing");
        let sources = HashMap::from([(record.id.to_string(), ModuleSource::Linked)]);
        let mut f = fixture(record, true, sources.clone(), None);
        f.resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources,
            linked: HashMap::from([(
                record.id.to_string(),
                (|| Err(crate::Error::Transport("secret path /home/x".into()))) as LinkedFactory,
            )]),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: None,
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(f.sink.clone())),
        });
        let error = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(
            error.contains("its linked factory could not be started"),
            "{error}"
        );
        assert!(!error.contains("/home/x"), "{error}");
    }

    #[tokio::test]
    async fn the_resolved_hook_runs_once_before_waiters_are_released() {
        let record = record("resolver-hook");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = seen.clone();
        let sink = Arc::new(Recording::default());
        let resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources: HashMap::new(),
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: Some(Arc::new(move |id| recorded.lock().unwrap().push(id.into()))),
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(sink)),
        });
        let _ = resolver.ensure_loaded(record.id).await;
        let _ = resolver.ensure_loaded(record.id).await;
        assert_eq!(seen.lock().unwrap().as_slice(), &[record.id.to_string()]);
    }

    unsafe extern "C" fn never_initialised(
        _: *const crate::module::abi::TbHostVtable,
        _: *mut crate::module::abi::TbModuleVtable,
    ) -> i32 {
        panic!("a mismatched linked module must be refused before init");
    }

    #[tokio::test]
    async fn a_linked_module_that_is_not_the_requested_record_is_refused_before_attach() {
        fn impostor() -> crate::Result<LinkedModule> {
            Ok(LinkedModule {
                descriptor: crate::module::abi::TbAbiDescriptor::current("impostor", "1.0.0"),
                manifest: super::module("Impostor", &[], &[]),
                init: never_initialised,
            })
        }
        let record = record("resolver-linked-impostor");
        let sources = HashMap::from([(record.id.to_string(), ModuleSource::Linked)]);
        let mut f = fixture(record, true, sources.clone(), None);
        f.resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources,
            linked: HashMap::from([(record.id.to_string(), impostor as LinkedFactory)]),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: None,
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(f.sink.clone())),
        });
        let error = f.resolver.ensure_loaded(record.id).await.unwrap_err();
        assert!(
            error.contains("does not match the registry record"),
            "{error}"
        );
        assert_eq!(f.resolver.state_of(record.id), Availability::Failed);
    }

    #[test]
    fn status_reflects_the_gate_and_a_local_source_on_an_unsupported_host() {
        let record = record("resolver-status-policy");
        let sources = HashMap::from([(record.id.to_string(), ModuleSource::Linked)]);
        // No asset is published for this host, but a linked source can serve it.
        let f = fixture(record, true, sources, None);
        assert_eq!(
            f.resolver.status_of(record),
            (Availability::Available, None)
        );

        let gated = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources: HashMap::new(),
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: Some(Arc::new(|_| Some("test-switch"))),
            on_resolved: None,
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(f.sink.clone())),
        });
        let (state, detail) = gated.status_of(record);
        assert_eq!(state, Availability::Disabled);
        assert!(detail.unwrap().contains("test-switch"));
    }

    #[tokio::test]
    async fn a_panicking_resolved_hook_does_not_strand_waiters() {
        let record = record("resolver-hook-panics");
        let sink = Arc::new(Recording::default());
        let resolver = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources: HashMap::new(),
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: Some(Arc::new(|_| panic!("hook bug"))),
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(|| Box::pin(async { Ok(shared_runtime()) })),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(sink)),
        });
        let outcome = resolver
            .ensure_loaded_within(record.id, Some(Duration::from_secs(10)))
            .await;
        assert!(matches!(outcome, Err(LoadError::Failed(_))), "{outcome:?}");
        assert_eq!(resolver.state_of(record.id), Availability::Failed);
    }

    #[tokio::test]
    async fn a_client_over_a_resolver_is_unavailable_and_reports_when_modules_are_disabled() {
        use crate::module::{ModuleCallError, ModuleClient};
        let record = record("resolver-client-disabled");
        let f = fixture(record, false, HashMap::new(), None);
        let client = ModuleClient::new(f.resolver.clone());
        let result = client.call::<()>(record.id, "Anything", ()).await;
        assert_eq!(result, Err(ModuleCallError::Unavailable));
        assert_eq!(
            f.sink.0.lock().unwrap().as_slice(),
            &[(record.id.to_string(), Reason::Disabled)]
        );
        assert_eq!(f.runtime_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_access_trait_delegates_to_the_resolver() {
        use crate::module::{ModuleAccess, ProxyFailure};
        let record = record("resolver-access");
        let f = fixture(record, true, HashMap::new(), None);
        let access: &dyn ModuleAccess = &f.resolver;
        assert_eq!(access.find(record.id), Some(record));
        assert!(access.find("nobody").is_none());
        assert!(access.enabled());
        access.reporter().report_unknown_module();
        // A valid bus address yields a proxy once the runtime is up.
        assert!(
            access
                .proxy("ai.tinyhumans.test.Access", "/ai/tinyhumans/test/Access")
                .await
                .is_ok()
        );
        // A malformed address is a contract failure, not a transport one.
        assert_eq!(
            access.proxy("not a bus name", "/nope").await.err(),
            Some(ProxyFailure::Contract)
        );
        // Loading goes through the ladder and fails here for lack of an artifact.
        assert!(access.ensure_loaded(record.id).await.is_err());

        let failing = Resolver::new(ResolverInputs {
            enabled: true,
            install_dir: None,
            allow_download: false,
            bundled_dir: None,
            sources: HashMap::new(),
            linked: HashMap::new(),
            search_skip: Vec::new(),
            gate: None,
            on_resolved: None,
            records: vec![record],
            terminal_marker: String::new(),
            runtime: Arc::new(|| {
                Box::pin(async { Err(crate::Error::Transport("no runtime".to_string())) })
            }),
            module_config: Arc::new(|_| serde_json::json!({})),
            reporter: Arc::new(FailureReporter::new(f.sink.clone())),
        });
        assert_eq!(
            ModuleAccess::proxy(
                &failing,
                "ai.tinyhumans.test.Access",
                "/ai/tinyhumans/test/Access"
            )
            .await
            .err(),
            Some(ProxyFailure::Transport)
        );
    }

    #[test]
    fn mark_terminal_is_idempotent() {
        let record = record("resolver-marker");
        let f = fixture(record, true, HashMap::new(), None);
        let once = f.resolver.mark_terminal("boom".to_string());
        assert_eq!(
            once,
            "boom. This is terminal for the running process; restart the app to try again"
        );
        assert_eq!(f.resolver.mark_terminal(once.clone()), once);
    }

    #[test]
    fn wire_forms_of_source_and_availability_are_stable() {
        assert_eq!(
            serde_json::to_value(ModuleSource::Pinned).unwrap(),
            serde_json::json!({"kind": "pinned"})
        );
        assert_eq!(
            serde_json::to_value(ModuleSource::LocalFile { path: "/x".into() }).unwrap(),
            serde_json::json!({"kind": "local_file", "path": "/x"})
        );
        assert_eq!(
            serde_json::to_value(ModuleSource::Linked).unwrap(),
            serde_json::json!({"kind": "linked"})
        );
        for (state, wire) in [
            (Availability::Available, "available"),
            (Availability::Loading, "loading"),
            (Availability::Ready, "ready"),
            (Availability::Failed, "failed"),
            (Availability::Unsupported, "unsupported"),
            (Availability::Disabled, "disabled"),
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(wire)
            );
        }
    }
}
