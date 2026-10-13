use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
