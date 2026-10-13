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
            gate: Some(Arc::new(|id| (id == "resolver-gated").then_some("test-switch"))),
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
        assert!(sink.0.lock().unwrap().is_empty(), "a switch is not a failure");
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
        assert_eq!(f.resolver.ensure_loaded(record.id).await.unwrap_err(), first);
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
        assert!(error.contains("its linked factory could not be started"), "{error}");
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
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(wire)
            );
        }
    }
}
