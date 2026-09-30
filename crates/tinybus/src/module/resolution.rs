//! Per-module resolution state, and the wait on it. Feature `modules`.
//!
//! A host's `ensure_loaded` used to hold one process-wide lock while it
//! resolved a module, and every caller for every module queued behind it. Three
//! things followed, and all three were visible in the field on a slow link:
//! the memory module waited its turn behind runtime downloads it had nothing to
//! do with; a caller that stopped waiting dropped the resolver's own future,
//! which released the lock with the module half-resolved and let the next
//! caller start a second download; and there was no way to say "still loading",
//! so every memory call ran into its caller's own timeout instead.
//!
//! This table replaces the lock with a slot per module. The first caller to
//! ask claims the slot and runs the resolution as a task with process lifetime,
//! so a caller that gives up cannot cancel it; every other caller waits on a
//! watch channel until the outcome lands. Waiting is cancel-safe, so a caller
//! may give up after a bound and report the module as loading rather than hang.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;

/// Outcome of resolving one module, remembered for the process lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Serving.
    Ready,
    /// Terminal. Carries the sanitised reason, never a path or URL.
    Failed(String),
}

/// What one module's slot holds.
#[derive(Debug, Clone)]
enum Slot {
    InFlight(watch::Receiver<Option<Resolution>>),
    Done(Resolution),
}

/// What [`ResolutionTable::claim`] hands the caller.
pub enum Claim {
    /// Nobody has asked for this module yet. The caller runs the resolution and
    /// reports through the sender; the receiver is its own seat in the queue.
    Run {
        /// Reports the outcome through [`ResolutionTable::complete`].
        sender: watch::Sender<Option<Resolution>>,
        /// The caller's own seat in the queue.
        receiver: watch::Receiver<Option<Resolution>>,
    },
    /// Someone else is resolving it: wait for their outcome.
    Wait(watch::Receiver<Option<Resolution>>),
    /// Already resolved, one way or the other.
    Done(Resolution),
}

/// A module's state as [`ResolutionTable::peek`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionState {
    /// Nothing has asked for it.
    Unresolved,
    /// Being downloaded, verified, or initialised.
    Loading,
    /// Serving.
    Ready,
    /// Terminal; carries the reason.
    Failed(String),
}

/// How a wait on a slot ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waited {
    /// The module is serving.
    Ready,
    /// Terminal; carries the reason.
    Failed(String),
    /// The bound passed before the outcome landed.
    StillLoading,
}

impl From<Resolution> for Waited {
    fn from(resolution: Resolution) -> Self {
        match resolution {
            Resolution::Ready => Self::Ready,
            Resolution::Failed(reason) => Self::Failed(reason),
        }
    }
}

/// One slot per module, keyed by registry id.
#[derive(Debug, Default)]
pub struct ResolutionTable {
    slots: Mutex<HashMap<String, Slot>>,
}

impl ResolutionTable {
    /// Claim `id` for resolution, or learn who already has.
    ///
    /// Atomic with respect to other claims: two concurrent first callers get one
    /// `Run` and one `Wait`, never two `Run`s — that is the property the old
    /// lock existed for, kept without serialising unrelated modules.
    pub fn claim(&self, id: &str) -> Claim {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slots.get(id) {
            Some(Slot::Done(resolution)) => Claim::Done(resolution.clone()),
            Some(Slot::InFlight(receiver)) => Claim::Wait(receiver.clone()),
            None => {
                let (sender, receiver) = watch::channel(None);
                slots.insert(id.to_string(), Slot::InFlight(receiver.clone()));
                Claim::Run { sender, receiver }
            }
        }
    }

    /// Record the outcome for `id` and wake everyone waiting on it.
    ///
    /// The table is updated before the channel fires, so a waiter that wakes and
    /// looks again sees the settled slot rather than a stale in-flight one.
    pub fn complete(
        &self,
        id: &str,
        resolution: Resolution,
        sender: watch::Sender<Option<Resolution>>,
    ) {
        {
            let mut slots = self
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slots.insert(id.to_string(), Slot::Done(resolution.clone()));
        }
        let _ = sender.send(Some(resolution));
    }

    /// Record that a module which resolved `Ready` has since faulted, such as
    /// when a lifecycle reinitialization is refused. tinybus never recovers a
    /// faulted module in-process, so the slot settles as failed and later
    /// callers get the reason instead of a retry.
    pub fn mark_faulted(&self, id: &str, reason: String) {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(slots.get(id), Some(Slot::Done(Resolution::Ready))) {
            slots.insert(id.to_string(), Slot::Done(Resolution::Failed(reason)));
        }
    }

    /// The state of `id` without touching it.
    pub fn peek(&self, id: &str) -> ResolutionState {
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slots.get(id) {
            None => ResolutionState::Unresolved,
            Some(Slot::InFlight(_)) => ResolutionState::Loading,
            Some(Slot::Done(Resolution::Ready)) => ResolutionState::Ready,
            Some(Slot::Done(Resolution::Failed(reason))) => ResolutionState::Failed(reason.clone()),
        }
    }

    /// Wait for an outcome on `receiver`, for at most `within` when given.
    ///
    /// Cancel-safe: dropping this future leaves the resolution running and the
    /// slot intact. A sender dropped without an outcome means the resolver task
    /// itself died before reporting; that is reported as a failure rather than
    /// waited on, because nothing will ever complete the slot.
    pub async fn wait(
        &self,
        id: &str,
        mut receiver: watch::Receiver<Option<Resolution>>,
        within: Option<Duration>,
    ) -> Waited {
        let id = id.to_string();
        let table = self;
        let settled = async move {
            loop {
                if let Some(resolution) = receiver.borrow_and_update().clone() {
                    return resolution;
                }
                if receiver.changed().await.is_err() {
                    let failure = Resolution::Failed(
                        "module resolution was abandoned; restart the app to try again".to_string(),
                    );
                    let mut slots = table
                        .slots
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if matches!(slots.get(&id), Some(Slot::InFlight(current)) if current.same_channel(&receiver))
                    {
                        slots.insert(id, Slot::Done(failure.clone()));
                    }
                    return failure;
                }
            }
        };
        match within {
            None => settled.await.into(),
            Some(limit) => match tokio::time::timeout(limit, settled).await {
                Ok(resolution) => resolution.into(),
                Err(_elapsed) => Waited::StillLoading,
            },
        }
    }

    /// Put `id` in flight without a resolver, handing back the sender that
    /// completes it. For host tests that need to observe the loading state.
    pub fn mark_in_flight(&self, id: &str) -> watch::Sender<Option<Resolution>> {
        let (sender, receiver) = watch::channel(None);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.insert(id.to_string(), Slot::InFlight(receiver));
        sender
    }

    /// Forget `id` entirely. For host tests, so a slot they planted does not
    /// outlive them in the process-wide table.
    pub fn forget(&self, id: &str) {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.remove(id);
    }
}

/// The process-wide table. One per process because tinybus loads a library at
/// most once per process, so the outcome is a fact about the process.
pub fn global() -> &'static ResolutionTable {
    static TABLE: OnceLock<ResolutionTable> = OnceLock::new();
    TABLE.get_or_init(ResolutionTable::default)
}

#[cfg(test)]
#[path = "resolution_test.rs"]
mod tests;
