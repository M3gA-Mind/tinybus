//! Opted-in instance terminal authority and bounded physical cleanup ownership.
use super::*;
use std::any::Any;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize};
use std::sync::{Condvar, Weak};
use tinybus::module::abi::{
    TB_NATIVE_TERMINAL_FAULT, TB_NATIVE_TERMINAL_LIVE, TB_NATIVE_TERMINAL_STOP_INTENT,
};

/// Cause of irreversible instance admission closure. Genuine fault takes
/// precedence over ordinary stop or host disappearance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalReason {
    Stop,
    HostClosed,
    Fault,
    SetupFailed,
}
/// Actual controller cleanup outcome, independent of native admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalCompletion {
    PhysicallyStopped,
    Uncertain,
}
/// Controller callbacks supplied by the terminal factory.
pub struct RuntimeHooks {
    request: Arc<dyn Fn(TerminalReason) + Send + Sync>,
    finish: Box<dyn FnOnce() -> TerminalCompletion + Send>,
}
impl RuntimeHooks {
    /// `request` must only close admission and signal bounded cancel/wake. It
    /// must not join, perform I/O, reenter SDK APIs, or wait for a gate. `finish`
    /// independently fences and joins the controller after SDK setup cancellation
    /// has settled; it must not depend on cancelled futures or host shutdown.
    /// Returning `PhysicallyStopped` asserts actual fence/join, not a timeout.
    pub fn new<R, F>(request: R, finish: F) -> Self
    where
        R: Fn(TerminalReason) + Send + Sync + 'static,
        F: FnOnce() -> TerminalCompletion + Send + 'static,
    {
        Self {
            request: Arc::new(request),
            finish: Box::new(finish),
        }
    }
}
/// A retained view of one actual admitted native instance. Its lifetime retains
/// the instance's admission permit even after successful physical cleanup.
#[derive(Clone)]
pub struct NativeInstanceLiveness {
    pub(super) inner: Arc<TerminalState>,
}
impl NativeInstanceLiveness {
    /// Only a supported, exactly-live native query allows new admission.
    /// Observed terminal state never reopens. This is not a publication lease.
    pub fn is_live(&self) -> bool {
        self.inner.is_live()
    }
    /// Persist genuine fault and close independent native admission first.
    pub fn request_fault(&self) {
        self.inner.signal(TerminalReason::Fault);
    }
}
const INSTANCE_LIMIT: usize = 256;
#[derive(Default)]
struct Slot {
    reserved: bool,
    exhausted: bool,
    generation: usize,
    anchor: Option<Arc<TerminalState>>,
}
struct Admission {
    slots: StdMutex<[Slot; INSTANCE_LIMIT]>,
}
impl Admission {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: StdMutex::new(std::array::from_fn(|_| Slot::default())),
        })
    }
}
static ADMISSION: OnceLock<Arc<Admission>> = OnceLock::new();
struct SlotLease {
    admission: Weak<Admission>,
    index: usize,
}
impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(admission) = self.admission.upgrade() {
            let mut slots = admission
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // The permit ends only at actual State last-Drop, never at anchor retirement.
            slots[self.index].reserved = false;
        }
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum RequestState {
    Unregistered,
    Idle,
    Running,
    Done,
    Panicked,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum FinishState {
    Idle,
    Submitted,
    Running,
    Complete,
    SpawnFailed,
}
struct FinishJob {
    state: Weak<TerminalState>,
}
impl FinishJob {
    fn run(self: Arc<Self>, state: Arc<TerminalState>) {
        state.run_finish();
    }
}
type Payload = Box<dyn Any + Send>;
#[derive(Default)]
struct PrimaryPayloads {
    request: Option<Payload>,
    finish: Option<Payload>,
    initial: Option<Payload>,
    reinit: Option<Payload>,
    factory: Option<Payload>,
    startup: Option<Payload>,
    cleanup: Option<Payload>,
}
#[derive(Clone, Copy)]
pub(super) enum PayloadKind {
    Request,
    Finish,
    Initial,
    Reinit,
    Factory,
    Startup,
    Cleanup,
}
impl PrimaryPayloads {
    fn cell(&mut self, kind: PayloadKind) -> &mut Option<Payload> {
        match kind {
            PayloadKind::Request => &mut self.request,
            PayloadKind::Finish => &mut self.finish,
            PayloadKind::Initial => &mut self.initial,
            PayloadKind::Reinit => &mut self.reinit,
            PayloadKind::Factory => &mut self.factory,
            PayloadKind::Startup => &mut self.startup,
            PayloadKind::Cleanup => &mut self.cleanup,
        }
    }
}
struct Control {
    request: Option<Arc<dyn Fn(TerminalReason) + Send + Sync>>,
    finish: Option<Box<dyn FnOnce() -> TerminalCompletion + Send>>,
    request_state: RequestState,
    finish_state: FinishState,
    published: bool,
    completion: Option<TerminalCompletion>,
    job: Option<Arc<FinishJob>>,
    payloads: PrimaryPayloads,
}
pub(super) type Setup =
    Box<dyn FnOnce(Connection) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send>;
pub(super) type SetupFutureSlot =
    Arc<StdMutex<Option<Pin<Box<dyn Future<Output = Result<()>> + Send>>>>>;

/// Resources are attached here before the next fallible startup boundary.
/// Taking values is mechanical; all foreign destruction occurs off this lock.
pub(super) struct StartupResources {
    pub runtime: Option<tokio::runtime::Runtime>,
    pub setup: Option<Setup>,
    pub retained_factory: Option<Box<dyn Any + Send>>,
    pub config: Option<Box<dyn Any + Send>>,
    pub reinitializer_factory: Option<ReinitializeFactory>,
    pub reinitialize: Arc<StdMutex<Option<Reinitialize>>>,
    pub connection: Arc<StdMutex<Option<Connection>>>,
    pub inbound: Option<mpsc::Sender<IncomingFrame>>,
    pub initial: Option<tokio::task::JoinHandle<Result<()>>>,
    pub initial_future: SetupFutureSlot,
    pub reinit_future: SetupFutureSlot,
}
impl Default for StartupResources {
    fn default() -> Self {
        Self {
            runtime: None,
            setup: None,
            retained_factory: None,
            config: None,
            reinitializer_factory: None,
            reinitialize: Arc::new(StdMutex::new(None)),
            connection: Arc::new(StdMutex::new(None)),
            inbound: None,
            initial: None,
            initial_future: Arc::new(StdMutex::new(None)),
            reinit_future: Arc::new(StdMutex::new(None)),
        }
    }
}
pub(super) struct TerminalState {
    pub host: HostCalls,
    pub owner: Option<ExecutionOwner>,
    reason: AtomicU8,
    uncertain: AtomicBool,
    control: StdMutex<Control>,
    changed: Condvar,
    pub terminal_notify: tokio::sync::Notify,
    pub resources: StdMutex<StartupResources>,
    sdk_owners: AtomicUsize,
    vtable_published: AtomicBool,
    lease: SlotLease,
    token: usize,
}
/// Every surviving SDK transport/Connection/task retains this actual State.
/// A pending runtime teardown may therefore never recycle its slot.
pub(super) struct SdkOwner {
    state: Arc<TerminalState>,
}
impl Drop for SdkOwner {
    fn drop(&mut self) {
        self.state.sdk_owners.fetch_sub(1, Ordering::AcqRel);
    }
}
impl TerminalState {
    pub fn admit(host: HostCalls, owner: Option<ExecutionOwner>) -> Option<Arc<Self>> {
        Self::admit_in(ADMISSION.get_or_init(Admission::new), host, owner)
    }
    fn admit_in(
        admission: &Arc<Admission>,
        host: HostCalls,
        owner: Option<ExecutionOwner>,
    ) -> Option<Arc<Self>> {
        let mut slots = admission
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = slots.iter_mut().enumerate().find_map(|(index, slot)| {
            if slot.reserved || slot.exhausted {
                return None;
            }
            let generation = slot
                .generation
                .checked_add(1)
                .filter(|generation| *generation <= usize::MAX >> 8);
            match generation {
                Some(generation) => {
                    slot.generation = generation;
                    Some(index)
                }
                None => {
                    slot.exhausted = true;
                    None
                }
            }
        })?;
        slots[index].reserved = true;
        let token = (slots[index].generation << 8) | index;
        let state = Arc::new(Self {
            host,
            owner,
            reason: AtomicU8::new(0),
            uncertain: AtomicBool::new(false),
            control: StdMutex::new(Control {
                request: None,
                finish: None,
                request_state: RequestState::Unregistered,
                finish_state: FinishState::Idle,
                published: false,
                completion: None,
                job: None,
                payloads: PrimaryPayloads::default(),
            }),
            changed: Condvar::new(),
            terminal_notify: tokio::sync::Notify::new(),
            resources: StdMutex::new(StartupResources::default()),
            sdk_owners: AtomicUsize::new(0),
            vtable_published: AtomicBool::new(false),
            lease: SlotLease {
                admission: Arc::downgrade(admission),
                index,
            },
            token,
        });
        slots[index].anchor = Some(state.clone());
        Some(state)
    }
    /// Opaque address token only; no allocation or dereference occurs.
    pub fn context(&self) -> *mut c_void {
        std::ptr::with_exposed_provenance_mut(self.token)
    }
    pub fn lookup(context: *mut c_void) -> Option<Arc<Self>> {
        Self::lookup_in(ADMISSION.get()?, context.addr())
    }
    fn lookup_in(admission: &Arc<Admission>, token: usize) -> Option<Arc<Self>> {
        let generation = token >> 8;
        if generation == 0 {
            return None;
        }
        let slots = admission
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = &slots[token & (INSTANCE_LIMIT - 1)];
        if slot.generation != generation {
            return None;
        }
        // Strong exact State is acquired before releasing registry exclusion.
        slot.anchor
            .as_ref()
            .filter(|state| state.token == token)
            .cloned()
    }
    pub fn mark_vtable_published(&self) {
        self.vtable_published.store(true, Ordering::Release);
    }
    pub fn retain_uncertain_anchor(self: &Arc<Self>) {
        self.uncertain.store(true, Ordering::Release);
        if let Some(admission) = self.lease.admission.upgrade() {
            let mut slots = admission
                .slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let slot = &mut slots[self.lease.index];
            // This State's still-live lease forbids reuse while a C callback or
            // retained view performs the final genuine-fault recheck.
            if slot.generation == self.token >> 8 && slot.anchor.is_none() {
                slot.anchor = Some(self.clone());
            }
        }
    }
    pub fn view(self: &Arc<Self>) -> NativeInstanceLiveness {
        NativeInstanceLiveness {
            inner: self.clone(),
        }
    }
    pub fn sdk_owner(self: &Arc<Self>) -> SdkOwner {
        self.sdk_owners.fetch_add(1, Ordering::AcqRel);
        SdkOwner {
            state: self.clone(),
        }
    }
    pub fn is_terminal(&self) -> bool {
        self.reason.load(Ordering::Acquire) != 0
    }
    pub fn is_live(self: &Arc<Self>) -> bool {
        if self.is_terminal() {
            return false;
        }
        self.observe_host();
        !self.is_terminal()
    }
    // Cleanup must sample even when local Stop already closed admission: a real
    // later native Fault is orthogonal to that local admission predicate.
    pub fn observe_host(self: &Arc<Self>) {
        let flags = self
            .host
            .0
            .native_terminal_state
            .map(|query| unsafe { query(self.host.0.host_ctx) });
        if flags != Some(TB_NATIVE_TERMINAL_LIVE) {
            let reason = if flags.is_some_and(|flags| flags & TB_NATIVE_TERMINAL_FAULT != 0) {
                TerminalReason::Fault
            } else if flags.is_some_and(|flags| flags & TB_NATIVE_TERMINAL_STOP_INTENT != 0) {
                TerminalReason::Stop
            } else {
                TerminalReason::HostClosed
            };
            self.signal(reason);
        }
    }
    fn reason(&self) -> TerminalReason {
        match self.reason.load(Ordering::Acquire) {
            1 => TerminalReason::Stop,
            2 => TerminalReason::HostClosed,
            4 => TerminalReason::SetupFailed,
            _ => TerminalReason::Fault,
        }
    }
    fn commit(&self, reason: TerminalReason) {
        let next = match reason {
            TerminalReason::Stop => 1,
            TerminalReason::HostClosed => 2,
            TerminalReason::Fault => 3,
            TerminalReason::SetupFailed => 4,
        };
        let mut old = self.reason.load(Ordering::Acquire);
        while old == 0 || (old < 3 && next >= 3) {
            match self
                .reason
                .compare_exchange_weak(old, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(actual) => old = actual,
            }
        }
        self.terminal_notify.notify_waiters();
    }
    pub fn signal(self: &Arc<Self>, reason: TerminalReason) {
        if matches!(reason, TerminalReason::Fault | TerminalReason::SetupFailed) {
            self.host.fault();
        }
        self.commit(reason);
        self.run_request();
        self.schedule();
    }
    pub fn source_panic(self: &Arc<Self>, kind: PayloadKind, payload: Payload) {
        // Atomic authority fence must precede payload-cell locks and foreign Drop.
        self.host.fault();
        self.commit(TerminalReason::Fault);
        self.uncertain.store(true, Ordering::Release);
        self.keep_payload(kind, payload);
        self.run_request();
        self.schedule();
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(|poison| {
            self.host.fault();
            self.commit(TerminalReason::Fault);
            self.uncertain.store(true, Ordering::Release);
            poison.into_inner()
        })
    }
    fn keep_payload(&self, kind: PayloadKind, payload: Payload) {
        let mut control = self.lock();
        let cell = control.payloads.cell(kind);
        // Once/terminal admission prevents a second primary in this category.
        // A violated internal invariant cannot discard a foreign payload here.
        // Call sites admit this category once and deny all later invocation.
        // No assertion/unwind, replacement retry, or payload disposal path.
        *cell = Some(payload);
    }
    pub fn register(self: &Arc<Self>, hooks: RuntimeHooks) {
        {
            let mut control = self.lock();
            control.request = Some(hooks.request);
            control.finish = Some(hooks.finish);
            control.request_state = RequestState::Idle;
        }
        if self.is_terminal() {
            self.run_request();
            self.schedule();
        }
    }
    fn run_request(self: &Arc<Self>) {
        let request = {
            let mut control = self.lock();
            if !self.is_terminal() || control.request_state != RequestState::Idle {
                return;
            }
            control.request_state = RequestState::Running;
            control.request.clone()
        };
        let _scope = self.owner.as_ref().map(ExecutionOwner::enter);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            if let Some(request) = request {
                request(self.reason());
            }
        }));
        match outcome {
            Ok(()) => {
                self.lock().request_state = RequestState::Done;
                self.changed.notify_all();
            }
            Err(payload) => {
                self.host.fault();
                self.commit(TerminalReason::Fault);
                self.uncertain.store(true, Ordering::Release);
                self.lock().request_state = RequestState::Panicked;
                self.changed.notify_all();
                self.keep_payload(PayloadKind::Request, payload);
            }
        }
    }
    pub fn publish(self: &Arc<Self>) {
        self.lock().published = true;
        self.changed.notify_all();
        if self.is_terminal() {
            self.run_request();
            self.schedule();
        }
    }
    fn schedule(self: &Arc<Self>) {
        let job = {
            let mut control = self.lock();
            if control.finish_state != FinishState::Idle {
                return;
            }
            let job = Arc::new(FinishJob {
                state: Arc::downgrade(self),
            });
            control.job = Some(job.clone());
            control.finish_state = FinishState::Submitted;
            job
        };
        let state = job.state.upgrade().expect("anchored terminal state");
        if std::thread::Builder::new()
            .name("tinybus-terminal".into())
            .spawn(move || job.run(state))
            .is_err()
        {
            self.uncertain.store(true, Ordering::Release);
            let mut control = self.lock();
            control.finish_state = FinishState::SpawnFailed;
            control.completion = Some(TerminalCompletion::Uncertain);
            drop(control);
            self.changed.notify_all();
        }
    }
    fn remove_anchor(&self) {
        if let Some(admission) = self.lease.admission.upgrade() {
            let anchor = {
                let mut slots = admission
                    .slots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slots[self.lease.index].anchor.take()
            };
            drop(anchor);
        }
    }
    pub fn empty_factory_failure(self: &Arc<Self>) {
        // No callable/controller was transferred. Retained views still hold the permit.
        self.commit(TerminalReason::SetupFailed);
        self.remove_anchor();
    }
    fn run_finish(self: &Arc<Self>) {
        let _scope = self.owner.as_ref().map(ExecutionOwner::enter);
        let finish = {
            let mut control = self.lock();
            while !control.published
                || matches!(
                    control.request_state,
                    RequestState::Idle | RequestState::Running
                )
            {
                control = self
                    .changed
                    .wait(control)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            control.finish_state = FinishState::Running;
            control.finish.take()
        };
        let cleanup = catch_unwind(AssertUnwindSafe(|| self.settle_setup()));
        let setup_settled = match cleanup {
            Ok(settled) => settled,
            Err(payload) => {
                self.source_panic(PayloadKind::Cleanup, payload);
                false
            }
        };
        let completed = if setup_settled {
            if let Some(owner) = &self.owner {
                owner.stop();
            }
            match catch_unwind(AssertUnwindSafe(|| {
                finish.map_or(TerminalCompletion::PhysicallyStopped, |finish| finish())
            })) {
                Ok(completion) => completion,
                Err(payload) => {
                    self.source_panic(PayloadKind::Finish, payload);
                    TerminalCompletion::Uncertain
                }
            }
        } else {
            // If setup destruction could not be proved, keep the original uninvoked finish owner.
            self.lock().finish = finish;
            TerminalCompletion::Uncertain
        };
        let released = if completed == TerminalCompletion::PhysicallyStopped {
            match catch_unwind(AssertUnwindSafe(|| self.release_resources())) {
                Ok(released) => released,
                Err(payload) => {
                    self.source_panic(PayloadKind::Cleanup, payload);
                    false
                }
            }
        } else {
            false
        };
        self.observe_host();
        if !released || (self.has_fault() && self.vtable_published.load(Ordering::Acquire)) {
            self.uncertain.store(true, Ordering::Release);
        }
        let completion = if self.uncertain.load(Ordering::Acquire) {
            TerminalCompletion::Uncertain
        } else {
            completed
        };
        {
            let mut control = self.lock();
            control.finish_state = FinishState::Complete;
            control.completion = Some(completion);
        }
        self.changed.notify_all();
        if completion == TerminalCompletion::PhysicallyStopped {
            self.remove_anchor();
        }
    }
    fn settle_setup(self: &Arc<Self>) -> bool {
        let (initial, handle, reinitialize, future, reinit_future) = {
            let mut resources = self
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                resources.initial.take(),
                resources
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.handle().clone()),
                resources.reinitialize.clone(),
                resources.initial_future.clone(),
                resources.reinit_future.clone(),
            )
        };
        if let Some(initial) = initial {
            initial.abort();
            let Some(handle) = handle else {
                self.resources
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .initial = Some(initial);
                return false;
            };
            match handle.block_on(initial) {
                Ok(Ok(())) => {}
                Ok(Err(_error)) => self.signal(TerminalReason::SetupFailed),
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    if error.is_panic() {
                        self.source_panic(PayloadKind::Initial, error.into_panic());
                    } else {
                        self.signal(TerminalReason::SetupFailed);
                    }
                }
            }
        }
        let callback = reinitialize
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(owner) = &self.owner {
            owner.stop();
        }
        let future = future
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(future);
        drop(callback);
        let request = self.lock().request.take();
        drop(request);
        let future = reinit_future
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(future);
        true
    }
    fn release_resources(&self) -> bool {
        let (setup, factory, config, reinit_factory, connection, inbound) = {
            let mut resources = self
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                resources.setup.take(),
                resources.retained_factory.take(),
                resources.config.take(),
                resources.reinitializer_factory.take(),
                resources.connection.clone(),
                resources.inbound.take(),
            )
        };
        let connection = connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop((setup, factory, config, reinit_factory, connection, inbound));
        let runtime = self
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime
            .take();
        if let Some(runtime) = runtime {
            runtime.shutdown_timeout(Duration::from_secs(4));
        }
        // Timeout is not proof: any surviving SDK transport/task keeps State
        // and its resource count. Unknown surviving ownership stays anchored.
        self.sdk_owners.load(Ordering::Acquire) == 0
    }
    pub fn has_fault(&self) -> bool {
        self.reason.load(Ordering::Acquire) >= 3
    }
    pub fn mark_uncertain(&self) {
        self.uncertain.store(true, Ordering::Release);
    }
    pub fn wait(&self, deadline: Duration) -> TerminalCompletion {
        let control = self.lock();
        let (control, _) = self
            .changed
            .wait_timeout_while(control, deadline, |control| control.completion.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        control.completion.unwrap_or(TerminalCompletion::Uncertain)
    }
}
#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
