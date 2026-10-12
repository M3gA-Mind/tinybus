use super::*;
use crate::message::Message;
use crate::router::Router;
use std::sync::atomic::AtomicUsize;

fn endpoint() -> HostCallbackEndpoint {
    HostCallbackEndpoint {
        path: "/callback".parse().unwrap(),
        interface: "org.example.Callback".parse().unwrap(),
        members: vec!["Observe".parse().unwrap()],
    }
}
fn state(source: BusName, destination: BusName) -> Arc<CallbackState> {
    Arc::new(CallbackState {
        destination,
        endpoints: vec![endpoint()],
        module_transport: Mutex::new(None),
        lifetime: Mutex::new(Lifetime {
            active: true,
            used: true,
            revoked: false,
            module: Some(source),
            running: 0,
            served: vec![],
            registering: vec![],
        }),
        drained: Notify::new(),
    })
}
fn call(state: &CallbackState) -> Message {
    let endpoint = endpoint();
    let mut message = Message::method_call(
        state.destination.clone(),
        endpoint.path,
        endpoint.interface,
        endpoint.members[0].clone(),
        serde_json::json!([]),
    );
    message.header.sender = state.module_peer();
    message.header.confidential = true;
    message
}
fn kernel_fixture() -> (Router, Arc<CallbackState>, u64, u64) {
    // Private router behavior tests only; actual artifact admission is proved by
    // host_tests with the hash-verified cdylib, never by this table fixture.
    let mut router = Router::default();
    let (source_id, source) = router.attach(tokio::sync::mpsc::channel(8).0);
    let (destination_id, destination) = router.attach(tokio::sync::mpsc::channel(8).0);
    router.set_attestation_for_unique(
        &source,
        crate::attest::Attestation {
            name: "org.example.Module".parse().unwrap(),
            sha256: "a".repeat(64),
        },
    );
    let state = state(source, destination);
    router.add_host_callbacks(state.clone()).unwrap();
    (router, state, source_id, destination_id)
}
fn resolves(router: &Router, message: &Message) -> Result<tokio::sync::mpsc::Sender<Message>> {
    router.resolve_call(
        message.header.destination.as_ref().unwrap(),
        message.header.expected_responder.as_ref(),
        true,
        &message.header,
    )
}
#[test]
fn scoped_routes_match_every_field_and_never_attest_the_host() {
    let (router, state, _, _) = kernel_fixture();
    let message = call(&state);
    assert!(resolves(&router, &message).is_ok());
    assert!(router.attestation_of(&state.destination).is_none());
    assert!(router.resolve_attested(&state.destination).is_err());
    for field in 0..5 {
        let mut wrong = message.clone();
        match field {
            0 => wrong.header.sender = Some(":1.999".parse().unwrap()),
            1 => wrong.header.path = Some("/other".parse().unwrap()),
            2 => wrong.header.interface = Some("org.example.Other".parse().unwrap()),
            3 => wrong.header.member = Some("Other".parse().unwrap()),
            _ => wrong.header.expected_responder = Some(":1.999".parse().unwrap()),
        }
        assert!(resolves(&router, &wrong).is_err());
    }
}
#[test]
fn a_host_alias_and_a_second_verified_module_inherit_no_authority() {
    let (mut router, state, _, destination_id) = kernel_fixture();
    let (_, other) = router.attach(tokio::sync::mpsc::channel(8).0);
    router.set_attestation_for_unique(
        &other,
        crate::attest::Attestation {
            name: "org.example.Other".parse().unwrap(),
            sha256: "b".repeat(64),
        },
    );
    let mut message = call(&state);
    message.header.sender = Some(other);
    assert!(resolves(&router, &message).is_err());
    let alias: BusName = "org.example.Host".parse().unwrap();
    router.request_name(destination_id, alias.clone()).unwrap();
    message = call(&state);
    message.header.destination = Some(alias);
    assert!(resolves(&router, &message).is_err());
}
#[test]
fn detaching_either_peer_revokes_stale_guard_and_replacements() {
    for detach_source in [false, true] {
        let (mut router, state, source_id, destination_id) = kernel_fixture();
        router.detach(if detach_source {
            source_id
        } else {
            destination_id
        });
        assert!(!state.matches(&call(&state).header));
        assert_eq!(
            router
                .add_host_callbacks(state.clone())
                .unwrap_err()
                .wire_name(),
            "ai.tinyhumans.tinybus.Error.Failed"
        );
        let (_, replacement) = router.attach(tokio::sync::mpsc::channel(8).0);
        assert_ne!(Some(replacement), state.module_peer());
    }
}
#[test]
fn unverified_sources_cannot_be_enrolled_and_scopes_are_bounded() {
    let mut router = Router::default();
    let (_, source) = router.attach(tokio::sync::mpsc::channel(8).0);
    let (_, destination) = router.attach(tokio::sync::mpsc::channel(8).0);
    let state = state(source.clone(), destination);
    assert!(router.add_host_callbacks(state.clone()).is_err());
    router.set_attestation_for_unique(
        &source,
        crate::attest::Attestation {
            name: "org.example.Module".parse().unwrap(),
            sha256: "a".repeat(64),
        },
    );
    router.add_host_callbacks(state.clone()).unwrap();
    assert!(router.add_host_callbacks(state).is_err());
    for _ in 1..128 {
        let (_, source) = router.attach(tokio::sync::mpsc::channel(8).0);
        let (_, destination) = router.attach(tokio::sync::mpsc::channel(8).0);
        router.set_attestation_for_unique(
            &source,
            crate::attest::Attestation {
                name: "org.example.Module".parse().unwrap(),
                sha256: "a".repeat(64),
            },
        );
        router
            .add_host_callbacks(super::tests::state(source, destination))
            .unwrap();
    }
    let (_, source) = router.attach(tokio::sync::mpsc::channel(8).0);
    let (_, destination) = router.attach(tokio::sync::mpsc::channel(8).0);
    router.set_attestation_for_unique(
        &source,
        crate::attest::Attestation {
            name: "org.example.Module".parse().unwrap(),
            sha256: "a".repeat(64),
        },
    );
    let extra = super::tests::state(source, destination);
    assert!(router.add_host_callbacks(extra).is_err());
}

struct BlockingCallback {
    entered: tokio::sync::mpsc::Sender<()>,
    release: Arc<tokio::sync::Semaphore>,
    published: Arc<AtomicUsize>,
    guard: HostCallbackGuard,
}
#[async_trait]
impl Interface for BlockingCallback {
    fn name(&self) -> InterfaceName {
        endpoint().interface
    }
    fn members(&self) -> Vec<MemberName> {
        endpoint().members
    }
    async fn call(&self, _: &MemberName, _: serde_json::Value) -> Result<serde_json::Value> {
        Err(Error::failed("unverified"))
    }
    async fn call_with_context(
        &self,
        _: &MemberName,
        _: serde_json::Value,
        context: &CallContext,
    ) -> Result<serde_json::Value> {
        self.entered.send(()).await.unwrap();
        let _permit = self.release.acquire().await.unwrap();
        if !self.guard.authorizes(context) {
            return Err(Error::failed("revoked before publication"));
        }
        self.published.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::Value::Null)
    }
}
#[tokio::test]
async fn revoke_after_route_selection_blocks_dispatch_and_direct_unverified_dispatch() {
    let (mut router, state, _, _) = kernel_fixture();
    let message = call(&state);
    assert!(resolves(&router, &message).is_ok());
    let (entered, mut receiver) = tokio::sync::mpsc::channel(1);
    let guarded = GuardedInterface {
        endpoint: endpoint(),
        state: state.clone(),
        interface: BlockingCallback {
            entered,
            release: Arc::new(tokio::sync::Semaphore::new(1)),
            published: Arc::new(AtomicUsize::new(0)),
            guard: HostCallbackGuard(state.clone()),
        },
    };
    assert!(
        guarded
            .call(&endpoint().members[0], serde_json::json!([]))
            .await
            .is_err()
    );
    assert!(
        guarded
            .call_with_context(
                &endpoint().members[0],
                serde_json::json!([]),
                &CallContext::brokered(state.module_peer().as_ref(), false)
            )
            .await
            .is_err()
    );
    assert!(
        guarded
            .call_with_context(
                &endpoint().members[0],
                serde_json::json!([]),
                &CallContext::default()
            )
            .await
            .is_err()
    );
    router.revoke_host_callbacks(&state);
    assert!(
        guarded
            .call_with_context(
                &endpoint().members[0],
                serde_json::json!([]),
                &CallContext::brokered(state.module_peer().as_ref(), true)
            )
            .await
            .is_err()
    );
    assert!(receiver.try_recv().is_err());
    assert_eq!(HostCallbackGuard(state).running(), 0);
}
#[tokio::test]
async fn revoke_during_await_preserves_running_charge_until_actual_completion() {
    let (mut router, state, _, _) = kernel_fixture();
    let guard = HostCallbackGuard(state.clone());
    let (entered, mut receiver) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let published = Arc::new(AtomicUsize::new(0));
    let guarded = GuardedInterface {
        endpoint: endpoint(),
        state: state.clone(),
        interface: BlockingCallback {
            entered,
            release: release.clone(),
            published: published.clone(),
            guard: guard.clone(),
        },
    };
    let context = CallContext::brokered(state.module_peer().as_ref(), true);
    let task = tokio::spawn(async move {
        guarded
            .call_with_context(&endpoint().members[0], serde_json::json!([]), &context)
            .await
    });
    receiver.recv().await.unwrap();
    assert_eq!(guard.running(), 1);
    router.revoke_host_callbacks(&state);
    assert_eq!(guard.running(), 1);
    let mut drain = Box::pin(guard.drained());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx).is_pending()))
            .await
    );
    release.add_permits(1);
    assert!(task.await.unwrap().is_err());
    guard.drained().await;
    assert_eq!(guard.running(), 0);
    assert_eq!(published.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn endpoint_declarations_registration_and_broker_ownership_are_checked() {
    let host = ModuleHost::new(Broker::new());
    assert!(host.prepare_host_callbacks(vec![]).await.is_err());
    assert!(
        host.prepare_host_callbacks(vec![endpoint(); 33])
            .await
            .is_err()
    );
    assert!(
        host.prepare_host_callbacks(vec![endpoint(), endpoint()])
            .await
            .is_err()
    );
    let mut duplicate = endpoint();
    duplicate.members.push(duplicate.members[0].clone());
    assert!(host.prepare_host_callbacks(vec![duplicate]).await.is_err());
    let mut excessive = endpoint();
    excessive.members = (0..65).map(|i| format!("M{i}").parse().unwrap()).collect();
    assert!(host.prepare_host_callbacks(vec![excessive]).await.is_err());
    let binding = host.prepare_host_callbacks(vec![endpoint()]).await.unwrap();
    assert!(
        host.load_file_with_host_callbacks("unused", serde_json::json!({}), &binding)
            .await
            .is_err()
    );
    let other_owner = ModuleHost::new(host.inner.broker.clone());
    assert!(
        other_owner
            .load_file_with_host_callbacks("unused", serde_json::json!({}), &binding)
            .await
            .is_err()
    );
    let other = ModuleHost::new(Broker::new());
    assert!(
        other
            .load_file_with_host_callbacks("unused", serde_json::json!({}), &binding)
            .await
            .is_err()
    );
    binding.revoke();
    binding.revoke();
    assert!(
        host.load_file_with_host_callbacks("unused", serde_json::json!({}), &binding)
            .await
            .is_err()
    );
    binding.close().await.unwrap();
}

struct EmptyCallback;
#[async_trait]
impl Interface for EmptyCallback {
    fn name(&self) -> InterfaceName {
        endpoint().interface
    }
    fn members(&self) -> Vec<MemberName> {
        vec![]
    }
    async fn call(&self, _: &MemberName, _: serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}
#[tokio::test]
async fn registration_is_exact_single_use_and_failed_load_detaches_the_local_peer() {
    let broker = Broker::new();
    let host = ModuleHost::new(broker.clone());
    let binding = host.prepare_host_callbacks(vec![endpoint()]).await.unwrap();
    assert!(
        binding
            .serve_at("/other".parse().unwrap(), EmptyCallback)
            .await
            .is_err()
    );
    assert!(
        binding
            .serve_at(endpoint().path, EmptyCallback)
            .await
            .is_err()
    );
    let (entered, _) = tokio::sync::mpsc::channel(1);
    let callback = || BlockingCallback {
        entered: entered.clone(),
        release: Arc::new(tokio::sync::Semaphore::new(1)),
        published: Arc::new(AtomicUsize::new(0)),
        guard: binding.guard(),
    };
    binding.serve_at(endpoint().path, callback()).await.unwrap();
    assert!(binding.serve_at(endpoint().path, callback()).await.is_err());
    assert_eq!(
        binding.unique_name(),
        &broker
            .router
            .lock()
            .unwrap()
            .owner_of(binding.unique_name())
            .unwrap()
    );
    assert!(
        host.load_file_with_host_callbacks("missing-fixture", serde_json::json!({}), &binding)
            .await
            .is_err()
    );
    assert!(
        broker
            .router
            .lock()
            .unwrap()
            .owner_of(binding.unique_name())
            .is_none()
    );
    assert!(binding.serve_at(endpoint().path, callback()).await.is_err());
    assert!(
        host.load_file_with_host_callbacks("missing-fixture", serde_json::json!({}), &binding)
            .await
            .is_err()
    );
    let peer = binding.unique_name().clone();
    drop(binding);
    assert!(broker.router.lock().unwrap().owner_of(&peer).is_none());
}

#[tokio::test]
async fn a_module_fault_revokes_dispatch_authority_before_peer_detach() {
    let (router, state, _, _) = kernel_fixture();
    let (transport, host) = ModuleTransport::new("fault-fixture".into(), Vec::new());
    *state.module_transport.lock().unwrap() = Some(Arc::downgrade(&transport));
    assert!(resolves(&router, &call(&state)).is_ok());
    unsafe {
        (host.fault)(host.host_ctx, std::ptr::null(), 0);
    }
    assert!(!state.matches(&call(&state).header));
    assert!(
        !HostCallbackGuard(state.clone())
            .authorizes(&CallContext::brokered(state.module_peer().as_ref(), true))
    );
    assert!(resolves(&router, &call(&state)).is_err());
}

#[tokio::test]
async fn restricted_outgoing_handle_preserves_typed_calls_and_metadata() {
    let broker = Broker::new();
    let host = ModuleHost::new(broker.clone());
    let binding = host.prepare_host_callbacks(vec![endpoint()]).await.unwrap();
    let outgoing = binding
        .proxy(crate::BUS_NAME, crate::BUS_PATH, crate::BUS_NAME)
        .unwrap()
        .with_timeout(Duration::from_secs(1));
    assert_eq!(outgoing.destination().as_str(), crate::BUS_NAME);
    assert_eq!(outgoing.path().as_str(), crate::BUS_PATH);
    assert_eq!(outgoing.interface().as_str(), crate::BUS_NAME);
    let names: Vec<BusName> = outgoing.clone().call("ListNames", ()).await.unwrap();
    assert!(names.contains(binding.unique_name()));
    let streamed: Vec<BusName> = outgoing.call_streaming("ListNames", ()).await.unwrap();
    assert_eq!(names, streamed);
    assert!(
        outgoing
            .call_confidential::<serde_json::Value>("ListNames", ())
            .await
            .is_err()
    );
    assert!(
        outgoing
            .call_sensitive::<serde_json::Value>("ListNames", ())
            .await
            .is_err()
    );
    assert!(outgoing.attestation().await.unwrap().is_none());
    assert!(!outgoing.is_available().await.unwrap());
    let _signals = outgoing.receive_signal("NameOwnerChanged").await.unwrap();
    assert!(format!("{outgoing:?}").contains("HostCallbackProxy"));
    binding.close().await.unwrap();
}

#[tokio::test]
async fn rollback_after_attach_releases_the_actual_initialization_observer() {
    let broker = Broker::new();
    let host = ModuleHost::new(broker);
    let binding = host.prepare_host_callbacks(vec![endpoint()]).await.unwrap();
    let manifest = super::super::tests::manifest();
    let path = Path::new("observer-fixture.so");
    let mut admitted = host
        .validate(path, &TbAbiDescriptor::current("clock", "0.1.0"), &manifest)
        .unwrap();
    admitted.state = ModuleState::Resolved;
    let (transport, _) = ModuleTransport::new(admitted.name.clone(), vec![]);
    transport.pause_initialization();
    let weak = Arc::downgrade(&transport);
    host.attach_transport(
        path,
        admitted,
        manifest,
        transport.clone(),
        Activation {
            lazy_init: true,
            lazy_load: false,
            descriptor_info: None,
            pinned_sha256: None,
        },
    )
    .unwrap();
    tokio::task::yield_now().await;
    drop(LoadRollback {
        binding: &binding,
        host: &host,
        transport: Some(transport.clone()),
        committed: false,
        cause: AdmissionTermination::Canceled,
    });
    assert!(!transport.init_started());
    assert!(host.list().is_empty());
    drop(transport);
    tokio::time::timeout(Duration::from_millis(100), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn configuration_fingerprints_preserve_json_equality_without_normalizing_strings() {
    fn fingerprint(value: &serde_json::Value) -> [u8; 32] {
        let bytes = crate::Secret::new(serde_json::to_vec(value).unwrap());
        callback_config_fingerprint(bytes.expose_secret())
    }
    let negative = serde_json::json!({
        "z": [-0.0, {"number": -0.0, "text": "\"-0.0\"\\-0.0"}],
        "credential": "private-test-secret",
    });
    // Construct the same map in reverse insertion order.
    let mut positive = serde_json::Map::new();
    positive.insert(
        "credential".into(),
        serde_json::json!("private-test-secret"),
    );
    positive.insert(
        "z".into(),
        serde_json::json!([
            0.0, {"number": 0.0, "text": "\"-0.0\"\\-0.0"}
        ]),
    );
    let positive = serde_json::Value::Object(positive);
    assert_eq!(negative, positive);
    assert_eq!(fingerprint(&negative), fingerprint(&positive));
    for differing in [
        serde_json::json!({"z": [0, {"number": 0.0, "text": "\"-0.0\"\\-0.0"}], "credential": "private-test-secret"}),
        serde_json::json!({"z": [0.0, {"number": 0.0, "text": "\"0.0\"\\-0.0"}], "credential": "private-test-secret"}),
        serde_json::json!({"z": [0.0, {"number": 0.0, "text": "\"-0.0\"\\-0.0"}], "credential": "different-private-test-secret"}),
    ] {
        assert_ne!(negative, differing);
        assert_ne!(fingerprint(&negative), fingerprint(&differing));
    }
    assert_eq!(
        fingerprint(&serde_json::json!(-0.0)),
        fingerprint(&serde_json::json!(0.0))
    );
    assert_ne!(
        fingerprint(&serde_json::json!(-0.01)),
        fingerprint(&serde_json::json!(0.01))
    );
}

// Private fixture-only release ownership. Drop never joins or asserts.
struct ControlledRelease {
    barrier: Arc<super::super::super::transport::tests::NativeBarrier>,
    finish: Option<std::sync::mpsc::Sender<()>>,
    resume: Option<tokio::sync::oneshot::Sender<()>>,
    transport: Option<Arc<ModuleTransport>>,
}
impl ControlledRelease {
    fn new(barrier: Arc<super::super::super::transport::tests::NativeBarrier>) -> Self {
        Self {
            barrier,
            finish: None,
            resume: None,
            transport: None,
        }
    }
}
impl ControlledRelease {
    fn release(&mut self) {
        self.barrier
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        self.barrier.changed.notify_all();
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
        if let Some(finish) = self.finish.take() {
            let _ = finish.send(());
        }
    }
}
impl Drop for ControlledRelease {
    fn drop(&mut self) {
        self.release();
        // A failing test cannot join from Drop. The existing transport physical
        // anchor remains authoritative; this task retains the real owner while
        // requesting stop after all artificial gates have opened.
        if let Some(transport) = self.transport.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let barrier = self.barrier.clone();
                runtime.spawn(async move {
                    let _ = transport.stop(Duration::from_secs(2)).await;
                    drop(barrier);
                });
            }
        }
    }
}

#[test]
fn controlled_release_wakes_entry_and_releases_channels_on_early_unwind() {
    use super::super::super::transport::tests::NativeBarrier;
    let barrier = Arc::new(NativeBarrier {
        state: Mutex::new((None, false, false)),
        changed: std::sync::Condvar::new(),
        entered: Notify::new(),
        joined: Notify::new(),
        finish_gate: Mutex::new(None),
    });
    let (finish, receive) = std::sync::mpsc::channel();
    let (resume, decision) = tokio::sync::oneshot::channel();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut cleanup = ControlledRelease::new(barrier.clone());
        cleanup.finish = Some(finish);
        cleanup.resume = Some(resume);
        panic!("fixture assertion before controlled release");
    }));
    let finish_result = receive.try_recv();
    let mut decision = decision;
    let decision_result = decision.try_recv();
    assert!(result.is_err());
    assert!(
        barrier.state.lock().unwrap().1,
        "early unwind must wake native entry"
    );
    assert!(
        finish_result.is_ok(),
        "finish must be released, not disconnected"
    );
    assert!(
        decision_result.is_ok(),
        "decision must be released, not disconnected"
    );
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_early_unwind_releases_entry_and_reclaims_after_callback_return() {
    use super::super::super::transport::tests::native_fixture;
    let (transport, host, init, barrier) = native_fixture();
    let weak = Arc::downgrade(&barrier);
    let mut cleanup = ControlledRelease::new(barrier.clone());
    cleanup.transport = Some(transport.clone());
    let (finish, receive) = std::sync::mpsc::channel();
    cleanup.finish = Some(finish);
    *barrier.finish_gate.lock().unwrap() = Some(receive);
    transport.defer_initialize(init, host);
    let initializing = tokio::spawn({
        let transport = transport.clone();
        async move { transport.ensure_initialized().await }
    });
    let entered = tokio::time::timeout(Duration::from_secs(2), barrier.entered.notified()).await;
    // Deliberate ordinary Rust unwind exercises the guard while actual C entry
    // is blocked on its condition variable. No panic crosses a C boundary.
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _cleanup = cleanup;
        panic!("early controlled fixture failure");
    }));
    let initialized = initializing.await;
    let stopped = transport.clone().stop(Duration::from_secs(2)).await;
    let callback_entered_after_worker_join = barrier.state.lock().unwrap().2;
    drop(barrier);
    drop(transport);
    let reclaimed = tokio::time::timeout(Duration::from_secs(2), async {
        while weak.strong_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Stop completion, rather than callback entry or abort, proves actual return.
    assert!(entered.is_ok());
    assert!(unwind.is_err());
    assert!(initialized.is_ok());
    assert!(stopped.is_ok());
    assert!(callback_entered_after_worker_join);
    assert!(reclaimed.is_ok());
}

// Manually polling the public future lets ordinary shutdown commit while the
// loader is suspended; no scheduler race decides which cancellation is tested.
async fn native_callback_load_after_ordinary_stop(drop_load: bool, reject_gate: bool) {
    use super::super::super::transport::tests::{native_fixture, native_fixture_json};
    use std::future::Future;
    let (unused, _, _, barrier) = native_fixture();
    let config = native_fixture_json(&unused);
    drop(unused);
    let artifact =
        std::path::PathBuf::from(std::env::var_os("TINYBUS_TEST_CONTROLLED_INIT_MODULE").unwrap());
    let hash = crate::module::hash::file_hex(std::fs::File::open(&artifact).unwrap()).unwrap();
    let (_dir, artifact) = super::super::tests::staged_module(&artifact, &hash);
    let host = ModuleHost::new(Broker::new());
    let binding = host.prepare_host_callbacks(vec![endpoint()]).await.unwrap();
    *binding.fixture_controller.lock().unwrap() = Some(barrier.clone());
    let (entered, _) = tokio::sync::mpsc::channel(1);
    binding
        .serve_at(
            endpoint().path,
            BlockingCallback {
                entered,
                release: Arc::new(tokio::sync::Semaphore::new(0)),
                published: Arc::new(AtomicUsize::new(0)),
                guard: binding.guard(),
            },
        )
        .await
        .unwrap();
    let mut cleanup = ControlledRelease::new(barrier.clone());
    let mut load = Some(Box::pin(
        host.load_file_with_host_callbacks(&artifact, config, &binding),
    ));
    let first =
        std::future::poll_fn(|cx| std::task::Poll::Ready(load.as_mut().unwrap().as_mut().poll(cx)))
            .await;
    if let std::task::Poll::Ready(completion) = first {
        cleanup.release();
        drop(completion);
        drop(load.take()); // already completed, no pending owner is discarded
        panic!("native fixture completed before C entry");
    }
    let entered = tokio::time::timeout(Duration::from_secs(2), barrier.entered.notified()).await;
    if entered.is_err() {
        cleanup.release();
        let completion = load.take().unwrap().await;
        drop(completion);
        panic!("native fixture entry watchdog expired; loader completion consumed");
    }
    let transport = host.inner.loaded.lock().unwrap()[0].transport.clone();
    cleanup.transport = Some(transport.clone());
    let view = barrier.state.lock().unwrap().0.unwrap();
    barrier.state.lock().unwrap().1 = true;
    barrier.changed.notify_all();
    let initialized = transport.ensure_initialized().await;
    // Advance the public future to its real withheld-Ready observation.
    let withheld =
        std::future::poll_fn(|cx| std::task::Poll::Ready(load.as_mut().unwrap().as_mut().poll(cx)))
            .await;
    let was_withheld = withheld.is_pending();
    let (finish, receive) = std::sync::mpsc::channel();
    *barrier.finish_gate.lock().unwrap() = Some(receive);
    cleanup.finish = Some(finish);
    let stopping = transport.clone();
    let stop = tokio::spawn(async move { stopping.stop(Duration::from_secs(2)).await });
    tokio::time::timeout(Duration::from_secs(2), barrier.joined.notified())
        .await
        .unwrap();
    let stopped_flags = unsafe { view.native_terminal_state.unwrap()(view.host_ctx) };
    let load_result = if let std::task::Poll::Ready(result) = withheld {
        Some(result)
    } else if reject_gate {
        transport.release_initialization(false);
        drop(load.take());
        None
    } else if drop_load {
        drop(load.take());
        None
    } else {
        // Real not-ready branch awaits close, whose native return is held.
        std::future::poll_fn(|cx| match load.as_mut().unwrap().as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Ready(None),
            std::task::Poll::Ready(result) => std::task::Poll::Ready(Some(result)),
        })
        .await
    };
    let held_flags = unsafe { view.native_terminal_state.unwrap()(view.host_ctx) };
    let held_fault = transport.is_faulted();
    cleanup.release();
    let stopped = stop.await;
    let loaded = if let Some(result) = load_result {
        drop(load.take());
        Some(result)
    } else if let Some(load) = load {
        Some(load.await)
    } else {
        None
    };
    cleanup.transport = None;
    assert!(initialized.is_ok());
    assert!(was_withheld, "fixture withholds Ready");
    assert!(stopped.unwrap().is_ok());
    if let Some(result) = loaded {
        assert!(result.is_err());
    }
    assert_eq!(stopped_flags, 2);
    assert_eq!(held_flags, 2, "ordinary cancellation cannot invent Fault");
    assert!(!held_fault);
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        10
    );
    assert!(barrier.state.lock().unwrap().2);
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_callback_notready_after_ordinary_stop_preserves_cause() {
    native_callback_load_after_ordinary_stop(false, false).await;
}
#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_callback_load_drop_after_ordinary_stop_preserves_cause() {
    native_callback_load_after_ordinary_stop(true, false).await;
}
#[tokio::test]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_callback_gate_rejection_after_ordinary_stop_preserves_cause() {
    native_callback_load_after_ordinary_stop(false, true).await;
}

async fn served_native_binding(host: &ModuleHost) -> HostCallbackBinding {
    let binding = host.prepare_host_callbacks(vec![endpoint()]).await.unwrap();
    let (entered, _) = tokio::sync::mpsc::channel(1);
    binding
        .serve_at(
            endpoint().path,
            BlockingCallback {
                entered,
                release: Arc::new(tokio::sync::Semaphore::new(0)),
                published: Arc::new(AtomicUsize::new(0)),
                guard: binding.guard(),
            },
        )
        .await
        .unwrap();
    binding
}

async fn native_callback_decision_race(ready: bool, native_fault: bool) {
    use super::super::super::transport::tests::{native_fixture, native_fixture_json};
    let (unused, _, _, barrier) = native_fixture();
    let mut config = native_fixture_json(&unused);
    config["test_ready"] = serde_json::json!(ready);
    drop(unused);
    let artifact =
        std::path::PathBuf::from(std::env::var_os("TINYBUS_TEST_CONTROLLED_INIT_MODULE").unwrap());
    let hash = crate::module::hash::file_hex(std::fs::File::open(&artifact).unwrap()).unwrap();
    let (_dir, artifact) = super::super::tests::staged_module(&artifact, &hash);
    let host = Arc::new(ModuleHost::new(Broker::new()));
    let binding = Arc::new(served_native_binding(&host).await);
    *binding.fixture_controller.lock().unwrap() = Some(barrier.clone());
    let decision = Arc::new(Notify::new());
    let (resume, release) = tokio::sync::oneshot::channel();
    *binding.before_load_decision.lock().unwrap() = Some((decision.clone(), release));
    let mut cleanup = ControlledRelease::new(barrier.clone());
    cleanup.resume = Some(resume);
    let loading_host = host.clone();
    let loading_binding = binding.clone();
    let load = tokio::spawn(async move {
        loading_host
            .load_file_with_host_callbacks(artifact, config, &loading_binding)
            .await
    });
    // Keep the paused runtime runnable until actual C entry/publication. This
    // prevents virtual time from auto-advancing the initializer's deadline.
    while barrier.state.lock().unwrap().0.is_none() {
        tokio::task::yield_now().await;
    }
    let transport = host.inner.loaded.lock().unwrap()[0].transport.clone();
    cleanup.transport = Some(transport.clone());
    let view = barrier.state.lock().unwrap().0.unwrap();
    barrier.state.lock().unwrap().1 = true;
    barrier.changed.notify_all();
    // Poll until native initialization is genuinely published, without an idle
    // paused timer deciding the deadline before the native owner can run.
    while !transport.init_started() {
        tokio::task::yield_now().await;
    }
    // init_started precedes actual publication; the observer result is the proof.
    let initialized = transport.ensure_initialized();
    tokio::pin!(initialized);
    let initialization =
        std::future::poll_fn(
            |cx| match std::future::Future::poll(initialized.as_mut(), cx) {
                std::task::Poll::Ready(result) => std::task::Poll::Ready(result),
                std::task::Poll::Pending => {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            },
        )
        .await;
    if !ready {
        // Real wait_ready deadline, not a fake readiness flag. Let the public
        // future install its timer before advancing past the actual bound.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
    }
    decision.notified().await;
    let observed_ready = transport.is_ready();
    let (finish, receive) = std::sync::mpsc::channel();
    *barrier.finish_gate.lock().unwrap() = Some(receive);
    cleanup.finish = Some(finish);
    let stopping = transport.clone();
    let stop = tokio::spawn(async move { stopping.stop(Duration::from_secs(2)).await });
    while !barrier.state.lock().unwrap().2 {
        tokio::task::yield_now().await;
    }
    let stopped_flags = unsafe { view.native_terminal_state.unwrap()(view.host_ctx) };
    let mut fault_flags = None;
    if native_fault {
        unsafe {
            (view.fault)(view.host_ctx, std::ptr::null(), 0);
        }
        fault_flags = Some(unsafe { view.native_terminal_state.unwrap()(view.host_ctx) });
    }
    let resumed = cleanup.resume.take().unwrap().send(());
    // Rollback revokes before awaiting physical close or returning failure.
    while !binding.state.lifetime.lock().unwrap().revoked && !load.is_finished() {
        tokio::task::yield_now().await;
    }
    let held = unsafe { view.native_terminal_state.unwrap()(view.host_ctx) };
    cleanup.release();
    // Keep virtual time runnable through the actual opaque return.
    while !stop.is_finished() {
        tokio::task::yield_now().await;
    }
    let stopped = stop.await;
    let loaded = load.await;
    cleanup.transport = None;
    assert!(initialization.is_ok());
    assert!(resumed.is_ok());
    assert!(stopped.unwrap().is_ok());
    assert!(loaded.unwrap().is_err());
    assert_eq!(observed_ready, ready);
    assert_eq!(stopped_flags, 2);
    if let Some(flags) = fault_flags {
        assert_eq!(flags, 6);
    }
    assert_eq!(held, if ready && !native_fault { 2 } else { 6 });
    assert_eq!(
        unsafe { view.native_terminal_state.unwrap()(view.host_ctx) },
        if ready && !native_fault { 10 } else { 14 }
    );
    assert_eq!(transport.is_faulted(), !ready || native_fault);
}

#[tokio::test(start_paused = true)]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_ready_commit_cancellation_preserves_ordinary_cause() {
    native_callback_decision_race(true, false).await;
}
#[tokio::test(start_paused = true)]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_readiness_deadline_still_faults_after_ordinary_stop() {
    native_callback_decision_race(false, false).await;
}
#[tokio::test(start_paused = true)]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_native_fault_at_ready_commit_dominates_ordinary_stop() {
    native_callback_decision_race(true, true).await;
}

#[derive(Debug, PartialEq, Eq)]
enum AttachOutcome {
    Attached,
    LoadSucceeded,
    LoadRefused,
    LoadPanicked,
    LoadCanceled,
    WatchdogExpired,
}

async fn await_native_attach<T>(
    attached: &Notify,
    load: &mut tokio::task::JoinHandle<Result<T>>,
    completed: &mut Option<std::result::Result<Result<T>, tokio::task::JoinError>>,
) -> AttachOutcome {
    // A fixture precondition watchdog, not a product security/readiness TTL.
    // Abort requests cooperative cancellation; it cannot interrupt synchronous
    // admission or prove physical native quiescence. The caller owns cleanup.
    tokio::select! {
        biased;
        _ = attached.notified() => AttachOutcome::Attached,
        result = &mut *load => {
            let category = match &result {
                Ok(Ok(_)) => AttachOutcome::LoadSucceeded,
                Ok(Err(_)) => AttachOutcome::LoadRefused,
                Err(error) if error.is_cancelled() => AttachOutcome::LoadCanceled,
                Err(_) => AttachOutcome::LoadPanicked,
            };
            *completed = Some(result);
            category
        },
        _ = tokio::time::sleep(Duration::from_secs(60)) => {
            load.abort();
            AttachOutcome::WatchdogExpired
        }
    }
}

#[tokio::test(start_paused = true)]
async fn setup_attachment_wait_allows_preload_to_outlive_product_deadlines() {
    let attached = Arc::new(Notify::new());
    let observed = attached.clone();
    let (release, preload) = tokio::sync::oneshot::channel::<()>();
    let mut load = tokio::spawn(async move {
        let _ = preload.await;
        std::future::pending::<Result<()>>().await
    });
    let wait = tokio::spawn(async move {
        let mut completed = None;
        let outcome = await_native_attach(&observed, &mut load, &mut completed).await;
        load.abort();
        let result = match completed {
            Some(result) => result,
            None => load.await,
        };
        (outcome, result)
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    let waiting = !wait.is_finished();
    let released = release.send(());
    attached.notify_one();
    let joined = wait.await;
    assert!(waiting, "valid preload was mistaken for setup failure");
    assert!(released.is_ok());
    let (outcome, result) = joined.unwrap();
    assert_eq!(outcome, AttachOutcome::Attached);
    assert!(result.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn setup_attachment_wait_reports_an_early_load_failure() {
    let attached = Notify::new();
    let mut load = tokio::spawn(async { Err::<(), _>(Error::ConnectionClosed) });
    let mut completed = None;
    let outcome = await_native_attach(&attached, &mut load, &mut completed).await;
    let result = completed.unwrap();
    assert_eq!(outcome, AttachOutcome::LoadRefused);
    assert!(matches!(result, Ok(Err(Error::ConnectionClosed))));
}

#[tokio::test(start_paused = true)]
async fn setup_attachment_watchdog_cancels_a_stuck_preload() {
    let attached = Notify::new();
    let mut load = tokio::spawn(std::future::pending::<Result<()>>());
    let wait = tokio::spawn(async move {
        let mut completed = None;
        let outcome = await_native_attach(&attached, &mut load, &mut completed).await;
        // Actual task completion is consumed. This synthetic task is cooperative;
        // this is not evidence of synchronous/native cancellation or C return.
        let result = match completed {
            Some(result) => result,
            None => load.await,
        };
        (outcome, result)
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(61)).await;
    let (outcome, result) = wait.await.unwrap();
    assert_eq!(outcome, AttachOutcome::WatchdogExpired);
    assert!(result.unwrap_err().is_cancelled());
}

async fn native_callback_setup_failure_race(panic_setup: bool) {
    use super::super::super::transport::tests::{native_fixture, native_fixture_json};
    let (unused, _, _, barrier) = native_fixture();
    let config = native_fixture_json(&unused);
    drop(unused);
    let artifact =
        std::path::PathBuf::from(std::env::var_os("TINYBUS_TEST_CONTROLLED_INIT_MODULE").unwrap());
    let hash = crate::module::hash::file_hex(std::fs::File::open(&artifact).unwrap()).unwrap();
    let (_dir, artifact) = super::super::tests::staged_module(&artifact, &hash);
    let host = Arc::new(ModuleHost::new(Broker::new()));
    let binding = Arc::new(served_native_binding(&host).await);
    *binding.fixture_controller.lock().unwrap() = Some(barrier.clone());
    let attached = Arc::new(Notify::new());
    let seen = attached.clone();
    let (resume, release) = std::sync::mpsc::channel();
    let mut cleanup = ControlledRelease::new(barrier.clone());
    cleanup.finish = Some(resume);
    *binding.after_attach.lock().unwrap() = Some(Box::new(move || {
        seen.notify_one();
        let _ = release.recv();
        assert!(
            !panic_setup,
            "actual Rust unwind during host callback setup"
        );
    }));
    let loading_host = host.clone();
    let loading_binding = binding.clone();
    let mut load = tokio::spawn(async move {
        loading_host
            .load_file_with_host_callbacks(artifact, config, &loading_binding)
            .await
    });
    let mut completed = None;
    let attachment = await_native_attach(&attached, &mut load, &mut completed).await;
    if attachment != AttachOutcome::Attached {
        cleanup.release();
        // Report the original finite cause before cleanup: synchronous admission
        // cannot be interrupted by abort. Do not turn it into a readiness failure.
        eprintln!("native fixture attachment: {attachment:?}");
        if let Some(completion) = completed {
            drop(completion);
        } else if tokio::time::timeout(Duration::from_secs(2), &mut load)
            .await
            .is_err()
        {
            // Pending ownership is explicit, not a quiescence claim. The task
            // consumes eventual completion and keeps the real staging, binding,
            // host and controller alive; transport's physical anchor also remains.
            tokio::spawn(async move {
                let _owners = (_dir, host, binding, barrier);
                let _ = load.await;
            });
            panic!("native fixture attachment: {attachment:?}; cleanup pending");
        }
        panic!("native fixture attachment: {attachment:?}; loader completion consumed");
    }
    let transport = host.inner.loaded.lock().unwrap()[0].transport.clone();
    cleanup.transport = Some(transport.clone());
    // Actual ordinary lifecycle ownership, before the callback initialization
    // gate opens. This instance proves C never entered, rather than any join.
    let stopped = transport.clone().stop(Duration::from_secs(2)).await;
    let held_fault = transport.is_faulted();
    if !panic_setup {
        // Public revocation makes real setup return its concurrently-used error.
        binding.revoke();
    }
    cleanup.release();
    let result = load.await;
    cleanup.transport = None;
    assert!(stopped.is_ok());
    assert!(!held_fault);
    if panic_setup {
        assert!(result.unwrap_err().is_panic());
    } else {
        assert!(result.unwrap().is_err());
    }
    assert!(
        transport.is_faulted(),
        "genuine setup failure upgrades ordinary stop"
    );
    assert!(
        barrier.state.lock().unwrap().0.is_none(),
        "C initializer never entered"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_callback_setup_error_still_faults_after_ordinary_stop() {
    native_callback_setup_failure_race(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTROLLED_INIT_MODULE built cdylib"]
async fn actual_callback_setup_unwind_still_faults_after_ordinary_stop() {
    native_callback_setup_failure_race(true).await;
}
