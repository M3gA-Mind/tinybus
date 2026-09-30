//! Tests for the per-module resolution table.
//!
//! Every test builds its own table: the process-wide one is shared with the
//! rest of the suite, and a slot planted there would be visible to any test
//! that happens to ask for the same module id.

use std::time::Duration;

use super::{Claim, Resolution, ResolutionState, ResolutionTable, Waited, global};

fn run_claim(
    table: &ResolutionTable,
    id: &str,
) -> (
    tokio::sync::watch::Sender<Option<Resolution>>,
    tokio::sync::watch::Receiver<Option<Resolution>>,
) {
    match table.claim(id) {
        Claim::Run { sender, receiver } => (sender, receiver),
        Claim::Wait(_) | Claim::Done(_) => panic!("first claim must run the resolution"),
    }
}

#[test]
fn the_first_claim_runs_and_every_later_claim_waits() {
    let table = ResolutionTable::default();
    assert_eq!(table.peek("m"), ResolutionState::Unresolved);

    let (_sender, _receiver) = run_claim(&table, "m");
    assert_eq!(table.peek("m"), ResolutionState::Loading);
    assert!(matches!(table.claim("m"), Claim::Wait(_)));
    assert!(matches!(table.claim("m"), Claim::Wait(_)));
}

#[test]
fn modules_do_not_share_a_slot() {
    let table = ResolutionTable::default();
    let (_a, _ra) = run_claim(&table, "a");
    // A second module is not queued behind the first.
    assert!(matches!(table.claim("b"), Claim::Run { .. }));
}

#[tokio::test]
async fn an_outcome_reaches_every_waiter_and_is_remembered() {
    let table = std::sync::Arc::new(ResolutionTable::default());
    let (sender, own) = run_claim(&table, "m");
    let Claim::Wait(other) = table.claim("m") else {
        panic!("second claim waits");
    };

    let waiter_table = table.clone();
    let waiter = tokio::spawn(async move { waiter_table.wait("m", other, None).await });
    table.complete("m", Resolution::Ready, sender);

    assert_eq!(table.wait("m", own, None).await, Waited::Ready);
    assert_eq!(waiter.await.unwrap(), Waited::Ready);
    assert_eq!(table.peek("m"), ResolutionState::Ready);
    assert!(matches!(table.claim("m"), Claim::Done(Resolution::Ready)));
}

#[tokio::test]
async fn a_failure_is_terminal_and_carries_its_reason() {
    let table = ResolutionTable::default();
    let (sender, receiver) = run_claim(&table, "m");
    table.complete("m", Resolution::Failed("refused".to_string()), sender);

    assert_eq!(
        table.wait("m", receiver, None).await,
        Waited::Failed("refused".to_string())
    );
    assert_eq!(
        table.peek("m"),
        ResolutionState::Failed("refused".to_string())
    );
    assert!(matches!(
        table.claim("m"),
        Claim::Done(Resolution::Failed(reason)) if reason == "refused"
    ));
}

#[tokio::test]
async fn a_bounded_wait_returns_an_outcome_that_arrives_before_its_deadline() {
    let table = ResolutionTable::default();
    let (sender, receiver) = run_claim(&table, "m");
    table.complete("m", Resolution::Ready, sender);
    assert_eq!(
        table
            .wait("m", receiver, Some(Duration::from_secs(1)))
            .await,
        Waited::Ready
    );
}

#[tokio::test]
async fn a_bounded_wait_reports_still_loading_instead_of_hanging() {
    let table = ResolutionTable::default();
    let (sender, receiver) = run_claim(&table, "m");

    let started = std::time::Instant::now();
    let outcome = table
        .wait("m", receiver.clone(), Some(Duration::from_millis(20)))
        .await;
    assert_eq!(outcome, Waited::StillLoading);
    assert!(started.elapsed() < Duration::from_secs(5));

    // Giving up did not disturb the resolution: the slot is still in flight and
    // the outcome still arrives for a later, unbounded wait.
    assert_eq!(table.peek("m"), ResolutionState::Loading);
    table.complete("m", Resolution::Ready, sender);
    assert_eq!(table.wait("m", receiver, None).await, Waited::Ready);
}

#[tokio::test]
async fn a_resolver_that_dies_without_reporting_fails_its_waiters() {
    let table = ResolutionTable::default();
    let (sender, receiver) = run_claim(&table, "m");
    drop(sender);
    assert!(matches!(
        table.wait("m", receiver, None).await,
        Waited::Failed(reason) if reason.contains("abandoned")
    ));
    assert!(
        matches!(table.peek("m"), ResolutionState::Failed(reason) if reason.contains("abandoned"))
    );
    assert!(
        matches!(table.claim("m"), Claim::Done(Resolution::Failed(reason)) if reason.contains("abandoned"))
    );
}

#[test]
fn test_hooks_plant_and_remove_a_slot() {
    let table = ResolutionTable::default();
    let _sender = table.mark_in_flight("m");
    assert_eq!(table.peek("m"), ResolutionState::Loading);
    table.forget("m");
    assert_eq!(table.peek("m"), ResolutionState::Unresolved);
}

#[test]
fn the_process_wide_resolution_table_is_stable() {
    assert!(std::ptr::eq(global(), global()));
}

#[test]
fn a_ready_module_that_faults_settles_as_failed() {
    let table = ResolutionTable::default();
    // Nothing resolved yet: a fault report has nothing to downgrade.
    table.mark_faulted("m", "refused".into());
    assert_eq!(table.peek("m"), ResolutionState::Unresolved);

    let (sender, _receiver) = run_claim(&table, "m");
    table.mark_faulted("m", "refused".into());
    assert_eq!(table.peek("m"), ResolutionState::Loading);

    table.complete("m", Resolution::Ready, sender);
    table.mark_faulted("m", "configuration refused".into());
    assert_eq!(
        table.peek("m"),
        ResolutionState::Failed("configuration refused".into())
    );
    assert!(matches!(
        table.claim("m"),
        Claim::Done(Resolution::Failed(_))
    ));
}
