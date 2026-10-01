use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
struct Req(u32);
#[derive(Debug, PartialEq)]
struct Resp(String);

#[tokio::test]
async fn a_registered_handler_round_trips_a_typed_payload() {
    let registry = NativeRegistry::new();
    registry.register::<Req, Resp, _, _>("demo.double", |req| async move {
        Ok(Resp(format!("{}", req.0 * 2)))
    });

    let resp: Resp = registry.request("demo.double", Req(21)).await.unwrap();
    assert_eq!(resp, Resp("42".to_string()));
}

#[tokio::test]
async fn a_non_serializable_payload_passes_through_untouched() {
    // The property that justifies this surface existing at all: a live
    // channel and a trait object arrive on the far side still usable.
    trait Tool: Send + Sync {
        fn name(&self) -> &str;
    }
    struct Hammer;
    impl Tool for Hammer {
        fn name(&self) -> &str {
            "hammer"
        }
    }

    struct Request {
        tools: Arc<Vec<Box<dyn Tool>>>,
        progress: mpsc::Sender<String>,
    }

    let registry = NativeRegistry::new();
    registry.register::<Request, (), _, _>("demo.run", |req| async move {
        let name = req.tools[0].name().to_string();
        req.progress.send(name).await.map_err(|e| e.to_string())?;
        Ok(())
    });

    let (tx, mut rx) = mpsc::channel(1);
    registry
        .request::<Request, ()>(
            "demo.run",
            Request {
                tools: Arc::new(vec![Box::new(Hammer)]),
                progress: tx,
            },
        )
        .await
        .unwrap();

    assert_eq!(rx.recv().await.unwrap(), "hammer");
}

#[tokio::test]
async fn an_unregistered_method_names_itself() {
    let registry = NativeRegistry::new();
    let err = registry
        .request::<Req, Resp>("demo.missing", Req(1))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        NativeRequestError::UnregisteredHandler {
            method: "demo.missing".into()
        }
    );
}

#[tokio::test]
async fn a_type_mismatch_is_an_error_rather_than_a_transmute() {
    let registry = NativeRegistry::new();
    registry.register::<Req, Resp, _, _>("demo.typed", |_| async { Ok(Resp("x".into())) });

    // Right request type, wrong response type.
    let err = registry
        .request::<Req, u64>("demo.typed", Req(1))
        .await
        .unwrap_err();
    assert!(
        matches!(err, NativeRequestError::TypeMismatch { .. }),
        "{err}"
    );

    // Wrong request type.
    let err = registry
        .request::<String, Resp>("demo.typed", "nope".to_string())
        .await
        .unwrap_err();
    assert!(
        matches!(err, NativeRequestError::TypeMismatch { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_handler_error_reaches_the_caller_with_its_message() {
    let registry = NativeRegistry::new();
    registry.register::<Req, Resp, _, _>("demo.fail", |_| async { Err("no device".to_string()) });

    let err = registry
        .request::<Req, Resp>("demo.fail", Req(1))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        NativeRequestError::HandlerFailed {
            method: "demo.fail".into(),
            message: "no device".into()
        }
    );
}

#[tokio::test]
async fn re_registering_replaces_so_a_test_can_stub_production() {
    let registry = NativeRegistry::new();
    registry.register::<Req, Resp, _, _>("demo.m", |_| async { Ok(Resp("real".into())) });
    registry.register::<Req, Resp, _, _>("demo.m", |_| async { Ok(Resp("stub".into())) });

    let resp: Resp = registry.request("demo.m", Req(0)).await.unwrap();
    assert_eq!(resp, Resp("stub".to_string()));
    assert_eq!(registry.len(), 1);
}

#[test]
fn registry_introspection_and_debug_never_block_on_a_writer() {
    let registry = NativeRegistry::new();
    assert!(registry.is_empty());
    registry.register::<Req, Resp, _, _>("demo.z", |_| async { Ok(Resp("z".into())) });
    registry.register::<Req, Resp, _, _>("demo.a", |_| async { Ok(Resp("a".into())) });
    assert_eq!(registry.methods(), ["demo.a", "demo.z"]);
    assert!(format!("{registry:?}").contains("demo.a"));

    let _writer = registry.handlers.write().unwrap();
    assert!(format!("{registry:?}").contains("<locked>"));
}

#[test]
fn native_errors_render_their_operation_and_type_details() {
    assert!(
        NativeRequestError::UnregisteredHandler {
            method: "missing".into()
        }
        .to_string()
        .contains("missing")
    );
    assert!(
        NativeRequestError::TypeMismatch {
            method: "typed".into(),
            expected: "Expected",
            actual: "Actual",
        }
        .to_string()
        .contains("expected Expected, got Actual")
    );
    assert!(
        NativeRequestError::HandlerFailed {
            method: "failed".into(),
            message: "reason".into(),
        }
        .to_string()
        .contains("reason")
    );
}

#[test]
fn clearing_an_empty_or_populated_registry_leaves_no_handlers() {
    let registry = NativeRegistry::new();
    registry.clear();
    registry.register::<Req, Resp, _, _>("demo.clear", |_| async { Ok(Resp("x".into())) });
    registry.clear();
    assert!(registry.is_empty());
    assert!(!registry.is_registered("demo.clear"));
}

#[tokio::test]
async fn a_slow_handler_does_not_block_an_unrelated_dispatch() {
    // The reason the lock is dropped before the await. If it were held,
    // the fast call could not complete until the slow one did.
    let registry = NativeRegistry::new();
    let (release_tx, mut release_rx) = mpsc::channel::<()>(1);
    let release = Arc::new(tokio::sync::Mutex::new(release_rx.recv()));
    drop(release);

    registry.register::<Req, Resp, _, _>("demo.slow", |_| async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Ok(Resp("slow".into()))
    });
    registry.register::<Req, Resp, _, _>("demo.fast", |_| async { Ok(Resp("fast".into())) });

    let slow = {
        let registry = registry.clone();
        tokio::spawn(async move { registry.request::<Req, Resp>("demo.slow", Req(0)).await })
    };
    // Completes immediately despite the slow dispatch being in flight.
    let fast: Resp = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        registry.request("demo.fast", Req(0)),
    )
    .await
    .expect("the fast dispatch is not blocked")
    .unwrap();
    assert_eq!(fast, Resp("fast".to_string()));
    assert_eq!(slow.await.unwrap().unwrap(), Resp("slow".to_string()));
    let _ = release_tx;
}

#[tokio::test]
async fn a_poisoned_lock_does_not_disable_the_registry() {
    let registry = NativeRegistry::new();
    registry.register::<Req, Resp, _, _>("demo.m", |_| async { Ok(Resp("ok".into())) });

    // Poison the lock from another thread.
    let poisoner = registry.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.handlers.write().unwrap();
        panic!("poisoning the registry lock");
    })
    .join();

    // One unrelated panic must not take every later dispatch with it.
    let resp: Resp = registry.request("demo.m", Req(0)).await.unwrap();
    assert_eq!(resp, Resp("ok".to_string()));
}

#[test]
fn methods_are_listed_sorted_for_introspection() {
    let registry = NativeRegistry::new();
    registry.register::<Req, Resp, _, _>("b.method", |_| async { Ok(Resp(String::new())) });
    registry.register::<Req, Resp, _, _>("a.method", |_| async { Ok(Resp(String::new())) });
    assert_eq!(registry.methods(), vec!["a.method", "b.method"]);
    assert!(!registry.is_empty());
    registry.clear();
    assert!(registry.is_empty());
}

#[test]
fn registration_needs_no_async_runtime() {
    // Startup code registers from `Once::call_once` and plain `fn main`,
    // neither of which has a runtime. A `#[test]` has none either, so this
    // compiling and running *is* the assertion.
    let registry = NativeRegistry::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    registry.register::<Req, Resp, _, _>("demo.m", move |_| {
        let seen = seen.clone();
        async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(Resp(String::new()))
        }
    });
    assert!(registry.is_registered("demo.m"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
