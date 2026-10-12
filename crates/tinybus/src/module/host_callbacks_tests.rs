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
