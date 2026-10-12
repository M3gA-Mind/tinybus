//! Native lifecycle completion must account for captured callback work.
use super::*;

struct CommittingCallback {
    entered: tokio::sync::mpsc::Sender<()>,
    release: Arc<tokio::sync::Semaphore>,
    committed: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl crate::Interface for CommittingCallback {
    fn name(&self) -> crate::InterfaceName {
        "org.example.Callback".parse().unwrap()
    }
    fn members(&self) -> Vec<crate::MemberName> {
        vec!["Observe".parse().unwrap()]
    }
    async fn call(&self, _: &crate::MemberName, _: serde_json::Value) -> Result<serde_json::Value> {
        Err(Error::failed("authenticated callback required"))
    }
    async fn call_with_context(
        &self,
        _: &crate::MemberName,
        _: serde_json::Value,
        context: &crate::CallContext,
    ) -> Result<serde_json::Value> {
        assert!(context.is_confidential());
        assert!(context.authenticated_sender().is_some());
        self.entered.send(()).await.unwrap();
        let _release = self.release.acquire().await.unwrap();
        // A native port may already have admitted its transaction. Revoking
        // new dispatch cannot roll back that transaction's eventual commit.
        self.committed.fetch_add(1, Ordering::AcqRel);
        Ok(serde_json::json!("committed"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTEXT_MODULE to point at the built cdylib"]
async fn stopping_a_verified_module_drains_its_admitted_host_callback_before_native_shutdown() {
    let artifact = PathBuf::from(std::env::var_os("TINYBUS_TEST_CONTEXT_MODULE").unwrap());
    let hash = crate::module::hash::file_hex(std::fs::File::open(&artifact).unwrap()).unwrap();
    let (_dir, artifact) = staged_module(&artifact, &hash);
    let host = ModuleHost::new(Broker::new());
    let binding = host
        .prepare_host_callbacks(vec![crate::module::HostCallbackEndpoint {
            path: "/callback".parse().unwrap(),
            interface: "org.example.Callback".parse().unwrap(),
            members: vec!["Observe".parse().unwrap()],
        }])
        .await
        .unwrap();
    let (entered, mut receiver) = tokio::sync::mpsc::channel(1);
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let committed = Arc::new(AtomicUsize::new(0));
    binding
        .serve_at(
            "/callback".parse().unwrap(),
            CommittingCallback {
                entered,
                release: release.clone(),
                committed: committed.clone(),
            },
        )
        .await
        .unwrap();
    let loaded = host
        .load_file_with_host_callbacks(artifact, serde_json::json!({}), &binding)
        .await
        .unwrap();
    let proxy = binding
        .proxy(
            binding.guard().module_peer().unwrap().as_str(),
            "/context",
            "org.example.Context",
        )
        .unwrap()
        .with_timeout(Duration::from_secs(2));
    let request = tokio::spawn(async move {
        proxy
            .call_confidential::<serde_json::Value>("ConfidentialCallback", ())
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.guard().running(), 1);
    let transport = host.inner.loaded.lock().unwrap()[0].transport.clone();
    let stopping = host.inner.stop(&loaded.name, Duration::from_secs(2));
    tokio::pin!(stopping);
    let completed_early = std::future::poll_fn(|context| {
        std::task::Poll::Ready(std::future::Future::poll(stopping.as_mut(), context).is_ready())
    })
    .await;
    let shutdown_started_early = transport.stop_task_spawned();
    let still_running = binding.guard().running();
    // Always release the actual worker before asserting the intended RED.
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), binding.guard().drained())
        .await
        .unwrap();
    if !completed_early {
        stopping.await.unwrap();
    }
    let _outcome = request.await.unwrap();
    assert_eq!(committed.load(Ordering::Acquire), 1);
    assert_eq!(still_running, 1);
    assert!(
        !completed_early,
        "stop completed before admitted callback commit"
    );
    assert!(
        !shutdown_started_early,
        "native shutdown started before callback commit"
    );
}

#[tokio::test]
async fn cancelling_reinitialize_cannot_enable_a_module_while_native_callback_still_runs() {
    let _guard = FAKE_MODULE_TEST_LOCK.lock().await;
    let host = ModuleHost::new(Broker::new());
    let (release, receiver) = std::sync::mpsc::sync_channel(1);
    *BLOCKING_REINIT_RECEIVER
        .get_or_init(|| StdMutex::new(None))
        .lock()
        .unwrap() = Some(receiver);
    BLOCKING_REINIT_STARTED.store(false, Ordering::Release);
    unsafe {
        host.attach_raw(
            "clock.so",
            TbAbiDescriptor::current("clock", "0.1.0"),
            manifest(),
            blocking_reinitialize_init,
        )
    }
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while host.list()[0].state != ModuleState::Ready {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let inner = host.inner.clone();
    let reinitializing =
        tokio::spawn(async move { inner.reinitialize("clock", serde_json::json!({})).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !BLOCKING_REINIT_STARTED.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    reinitializing.abort();
    assert!(reinitializing.await.unwrap_err().is_cancelled());
    let disabled = host.inner.enable("clock", false);
    let enabled = host.inner.enable("clock", true);
    release.send(()).unwrap();
    host.reinitialize("clock", serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        disabled.is_err(),
        "disable changed admission during native reinit"
    );
    assert!(
        enabled.is_err(),
        "enable reopened admission during native reinit"
    );
}
