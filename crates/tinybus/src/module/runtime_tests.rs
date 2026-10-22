use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn setup_runs_before_the_runtime_is_handed_out() {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let setup: RuntimeSetup = Box::new(move |_connection| {
        Box::pin(async move {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
    });
    let runtime = ModuleRuntime::start(&RuntimeNames::default(), Some(setup)).unwrap();
    assert!(ran.load(Ordering::SeqCst));
    assert!(runtime.host().list().is_empty());
    assert!(runtime.connection().unique_name().is_some());
}

#[tokio::test]
async fn a_failing_setup_fails_the_start() {
    let setup: RuntimeSetup =
        Box::new(|_| Box::pin(async { Err(crate::Error::Transport("setup".to_string())) }));
    let result = ModuleRuntime::start(&RuntimeNames::default(), Some(setup));
    assert!(result.is_err());
}

#[tokio::test]
async fn proxies_validate_names_and_unloaded_modules_do_not_answer() {
    let runtime = ModuleRuntime::start(&RuntimeNames::default(), None).unwrap();
    let proxy = runtime
        .proxy("ai.tinyhumans.test.Nothing", "/ai/tinyhumans/test/Nothing")
        .unwrap();
    let result: crate::Result<serde_json::Value> = proxy.call("Anything", ()).await;
    assert!(result.is_err(), "an unloaded module must not answer");
    assert!(runtime.proxy("not a bus name", "/nope").is_err());
}

#[tokio::test]
async fn work_outlives_the_caller_runtime_and_blocking_reports_errors() {
    let runtime = ModuleRuntime::start(&RuntimeNames::default(), None).unwrap();
    let value = runtime.spawn(async { 7 }).await.unwrap();
    assert_eq!(value, 7);
    assert_eq!(runtime.blocking(|| Ok(())).await, Ok(()));
    assert_eq!(
        runtime.blocking(|| Err("refused".to_string())).await,
        Err("refused".to_string())
    );
    let panicked = runtime.blocking(|| panic!("loader panic")).await;
    assert!(panicked.unwrap_err().contains("did not finish"));
}

#[tokio::test]
async fn configured_worker_stack_supports_deep_synchronous_work() {
    const CHILD: &str = "TINYBUS_CONFIGURED_STACK_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = ModuleRuntime::start_with_options(
            &RuntimeNames::default(),
            None,
            RuntimeOptions {
                thread_stack_size: Some(4 * 1024 * 1024),
            },
        )
        .unwrap();
        let result = runtime
            .spawn(async { consume_worker_stack(96) })
            .await
            .unwrap();
        assert_ne!(result, 0);
        return;
    }

    // A stack overflow aborts the process, so isolate the regression check in
    // a child test process. Without the configured 4 MiB Tokio worker stack,
    // this 3 MiB recursive workload overflows Tokio's default worker stack.
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "module::runtime::tests::configured_worker_stack_supports_deep_synchronous_work",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "configured-stack child exited with {status}"
    );
}

#[inline(never)]
fn consume_worker_stack(depth: usize) -> usize {
    const FRAME_BYTES: usize = 32 * 1024;
    let frame = [depth as u8; FRAME_BYTES];
    std::hint::black_box(&frame);
    let value = if depth == 0 {
        0
    } else {
        consume_worker_stack(depth - 1)
    };
    value + usize::from(frame[depth % FRAME_BYTES])
}
