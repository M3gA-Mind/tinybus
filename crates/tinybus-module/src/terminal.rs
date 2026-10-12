//! Opted-in instance terminal authority and bounded physical cleanup ownership.
use super::*;
use tinybus::module::abi::TB_NATIVE_TERMINAL_LIVE;

/// Cause of irreversible instance admission closure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalReason { Stop, HostClosed, Fault, SetupFailed }
/// Actual controller cleanup outcome, independent of native admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCompletion { PhysicallyStopped, Uncertain }
/// Controller callbacks supplied by the terminal factory.
pub struct RuntimeHooks {
    request: Arc<dyn Fn(TerminalReason) + Send + Sync>,
    finish: Box<dyn FnOnce() -> TerminalCompletion + Send>,
}
impl RuntimeHooks {
    /// `request` must only close admission and signal bounded cancel/wake. It
    /// must not join, perform I/O, or wait for a gate. `finish` independently
    /// fences and joins the controller after SDK setup cancellation has settled;
    /// it must not depend on those cancelled futures or on host shutdown.
    pub fn new<R, F>(request: R, finish: F) -> Self
    where R: Fn(TerminalReason) + Send + Sync + 'static,
          F: FnOnce() -> TerminalCompletion + Send + 'static {
        Self { request: Arc::new(request), finish: Box::new(finish) }
    }
}
/// A retained view of one actual admitted native instance.
#[derive(Clone)]
pub struct NativeInstanceLiveness { inner: Arc<TerminalState> }
impl NativeInstanceLiveness {
    /// Only a supported, exactly-live native query allows new admission.
    pub fn is_live(&self) -> bool {
        if self.inner.closed.load(Ordering::Acquire) { return false; }
        let live = self.inner.host.0.native_terminal_state.is_some_and(|query| unsafe {
            query(self.inner.host.0.host_ctx) == TB_NATIVE_TERMINAL_LIVE
        });
        if !live { self.inner.closed.store(true, Ordering::Release); }
        live && !self.inner.closed.load(Ordering::Acquire)
    }
    /// Persist genuine fault and close independent native admission first.
    pub fn request_fault(&self) { self.inner.host.fault(); self.inner.closed.store(true, Ordering::Release); }
}
struct TerminalState { host: HostCalls, closed: AtomicBool }
#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
