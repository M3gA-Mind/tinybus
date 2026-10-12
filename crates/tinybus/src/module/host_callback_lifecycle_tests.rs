//! Native lifecycle completion must account for captured callback work.
use super::*;

struct CommittingCallback {
    entered: tokio::sync::mpsc::Sender<()>,
    release: Arc<tokio::sync::Semaphore>,
    committed: Arc<AtomicUsize>,
    context: Arc<StdMutex<Option<crate::CallContext>>>,
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
        *self.context.lock().unwrap() = Some(context.clone());
        self.entered.send(()).await.unwrap();
        let _release = self.release.acquire().await.unwrap();
        // A native port may already have admitted its transaction. Revoking
        // new dispatch cannot roll back that transaction's eventual commit.
        self.committed.fetch_add(1, Ordering::AcqRel);
        Ok(serde_json::json!("committed"))
    }
}

#[derive(Clone, Copy)]
enum DrainStop {
    Complete,
    Timeout,
    Cancel,
    DropHost,
}

struct CallbackRelease(Arc<tokio::sync::Semaphore>);
impl Drop for CallbackRelease {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTEXT_MODULE to point at the built cdylib"]
async fn stopping_a_verified_module_drains_its_admitted_host_callback_before_native_shutdown() {
    stop_after_admitted_callback(DrainStop::Complete).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTEXT_MODULE to point at the built cdylib"]
async fn host_stop_drain_timeout_retains_stop_intent_and_denies_new_admission() {
    stop_after_admitted_callback(DrainStop::Timeout).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTEXT_MODULE to point at the built cdylib"]
async fn host_stop_drain_cancellation_retains_stop_intent_and_denies_new_admission() {
    stop_after_admitted_callback(DrainStop::Cancel).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TINYBUS_TEST_CONTEXT_MODULE to point at the built cdylib"]
async fn host_drop_after_cancelled_stop_waits_for_actual_provider_drain() {
    stop_after_admitted_callback(DrainStop::DropHost).await;
}

async fn stop_after_admitted_callback(mode: DrainStop) {
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
    let context = Arc::new(StdMutex::new(None));
    binding
        .serve_at(
            "/callback".parse().unwrap(),
            CommittingCallback {
                entered,
                release: release.clone(),
                committed: committed.clone(),
                context: context.clone(),
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
    let release_on_failure = CallbackRelease(release.clone());
    let first_proxy = proxy.clone();
    let request = tokio::spawn(async move {
        first_proxy
            .call_confidential::<serde_json::Value>("ConfidentialCallback", ())
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let transport = host.inner.loaded.lock().unwrap()[0].transport.clone();
    let deadline = if matches!(mode, DrainStop::Timeout) {
        Duration::ZERO
    } else {
        Duration::from_secs(2)
    };
    let mut stopping = Some(Box::pin(host.inner.stop(&loaded.name, deadline)));
    let mut early = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(
            match std::future::Future::poll(stopping.as_mut().unwrap().as_mut(), cx) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            },
        )
    })
    .await;
    if !matches!(mode, DrainStop::Complete) {
        drop(stopping.take());
    }
    let terminal_while_held = transport.native_terminal_state_for_test();
    let shutdown_started_early = transport.stop_task_spawned();
    let physically_stopped_early =
        terminal_while_held & crate::module::abi::TB_NATIVE_TERMINAL_PHYSICALLY_STOPPED != 0;
    let still_running = binding.guard().running();
    let authority_denied = !binding
        .guard()
        .authorizes(context.lock().unwrap().as_ref().unwrap());
    let owner_retained = host
        .inner
        .loaded
        .lock()
        .unwrap()
        .iter()
        .any(|module| Arc::ptr_eq(&module.transport, &transport));
    let enable_denied = host.inner.enable(&loaded.name, true).is_err();
    let new_call_denied = proxy
        .call_confidential::<serde_json::Value>("ConfidentialCallback", ())
        .await
        .is_err();
    if matches!(mode, DrainStop::DropHost) {
        let account = host.inner.loaded.lock().unwrap()[0]
            .callback_state
            .clone()
            .unwrap();
        let host_owner = Arc::downgrade(&host.inner);
        drop(stopping);
        drop(host);
        let entered = tokio::time::timeout(
            Duration::from_secs(2),
            account.physical_wait_started.notified(),
        )
        .await;
        let terminal_after_drop = transport.native_terminal_state_for_test();
        let still_counted = binding.guard().running();
        let lifecycle_free = !transport.lifecycle_is_owned_for_test();
        drop(release_on_failure);
        tokio::time::timeout(Duration::from_secs(2), binding.guard().drained())
            .await
            .unwrap();
        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            while transport.native_terminal_state_for_test()
                & crate::module::abi::TB_NATIVE_TERMINAL_PHYSICALLY_STOPPED
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let _outcome = request.await;
        assert!(completed.is_ok());
        assert!(entered.is_ok());
        assert!(host_owner.upgrade().is_none());
        assert_eq!(still_counted, 1);
        assert_eq!(
            terminal_after_drop,
            crate::module::abi::TB_NATIVE_TERMINAL_STOP_INTENT
        );
        assert!(lifecycle_free && authority_denied && new_call_denied);
        assert_eq!(committed.load(Ordering::Acquire), 1);
        assert!(!transport.is_faulted());
        return;
    }
    // Release and consume real callback/physical cleanup before all assertions.
    drop(release_on_failure);
    tokio::time::timeout(Duration::from_secs(2), binding.guard().drained())
        .await
        .unwrap();
    let stopped = if let Some(stopping) = stopping {
        if let Some(result) = early.take() {
            result
        } else {
            stopping.await
        }
    } else {
        host.inner.stop(&loaded.name, Duration::from_secs(2)).await
    };
    let _outcome = request.await.unwrap();
    assert!(stopped.is_ok());
    assert_eq!(committed.load(Ordering::Acquire), 1);
    assert_eq!(still_running, 1);
    assert!(authority_denied && owner_retained && enable_denied && new_call_denied);
    assert!(!shutdown_started_early && !physically_stopped_early);
    assert_eq!(
        terminal_while_held,
        crate::module::abi::TB_NATIVE_TERMINAL_STOP_INTENT,
        "native authority must close before admitted provider work drains"
    );
    assert!(!transport.is_faulted());
    if matches!(mode, DrainStop::Timeout) {
        assert!(early.is_some_and(|result| result.is_err()));
    }
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
