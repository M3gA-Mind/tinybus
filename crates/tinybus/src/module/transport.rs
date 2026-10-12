//! Host-side transport bridge over the module C vtables.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, OnceCell, mpsc};

use crate::error::{Error, Result};
use crate::message::codec::MAX_FRAME_LEN;
use crate::message::{Message, MessageKind};
use crate::module::abi::{
    TB_BACKPRESSURE, TB_BAD_ARGUMENT, TB_CLOSED, TB_MODULE_VTABLE_BASE_SIZE, TB_OK, TB_PANICKED,
    TB_TIMEOUT, TbHostVtable, TbModuleVtable,
};
use crate::ports::Transport;

const HOST_QUEUE_CAPACITY: usize = 256;
const MODULE_INIT_DEADLINE: Duration = Duration::from_secs(5);

/// How long the host waits for a module to drain its queue before declaring it
/// faulted. Bounded so a module that stops calling `wake` cannot park the
/// delivery task for the process lifetime; the security boundary applies to
/// in-process modules too ("every call has a deadline").
const BACKPRESSURE_DEADLINE: Duration = Duration::from_secs(5);

struct HostContext {
    inbound: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    wake: Arc<Notify>,
    config: StdMutex<Vec<u8>>,
    faulted: AtomicBool,
    init_failed: AtomicBool,
    stopped: AtomicBool,
    init_started: AtomicBool,
    init_notify: Notify,
    ready: AtomicBool,
    broker_routing: AtomicBool,
    ready_notify: Notify,
    inflight: AtomicUsize,
}

/// The broker-facing side of one loaded module.
pub(crate) struct ModuleTransport {
    self_ref: Weak<ModuleTransport>,
    module: StdMutex<Option<TbModuleVtable>>,
    inbound: Mutex<mpsc::Receiver<Vec<u8>>>,
    context: &'static HostContext,
    label: String,
    initializer: StdMutex<Option<DeferredInitializer>>,
    init_result: OnceCell<std::result::Result<(), InitFailure>>,
    pending: StdMutex<VecDeque<Message>>,
    drain_started: AtomicBool,
    // Reconfiguration and shutdown both call into opaque module code. The
    // owned guard moves into the blocking task, so a host-side timeout cannot
    // let a second lifecycle callback overlap the still-running first one.
    lifecycle: Arc<Mutex<()>>,
    stop_task_spawned: Arc<AtomicBool>,
    initialization_gate: StdMutex<Option<bool>>,
    initialization_gate_changed: Notify,
}

// `module_ctx` is opaque and all access to it goes through callbacks whose ABI
// contract requires thread safety. The vtable itself is immutable after init.
unsafe impl Send for ModuleTransport {}
unsafe impl Sync for ModuleTransport {}

type Initializer =
    Box<dyn FnOnce(TbHostVtable) -> std::result::Result<TbModuleVtable, String> + Send + 'static>;

struct DeferredInitializer {
    initialize: Initializer,
    host: TbHostVtable,
}

struct SendModuleVtable(TbModuleVtable);

#[derive(Clone)]
enum InitFailure {
    Closed,
    Failed(String),
}

// The opaque host context is process-lifetime state and every callback is
// required by the ABI to be thread-safe.
unsafe impl Send for DeferredInitializer {}
unsafe impl Send for SendModuleVtable {}

impl SendModuleVtable {
    unsafe fn reinitialize(
        &self,
        callback: crate::module::abi::TbModuleReinitialize,
        bytes: &[u8],
    ) -> i32 {
        unsafe { callback(self.0.module_ctx, bytes.as_ptr(), bytes.len()) }
    }
}

impl DeferredInitializer {
    fn run(self) -> std::result::Result<SendModuleVtable, String> {
        (self.initialize)(self.host).map(SendModuleVtable)
    }
}

impl ModuleTransport {
    pub(crate) fn admit_broker(&self) {
        self.context.broker_routing.store(true, Ordering::Release);
    }

    pub(crate) fn new(label: String, config: Vec<u8>) -> (Arc<Self>, TbHostVtable) {
        let (inbound_tx, inbound_rx) = mpsc::channel(HOST_QUEUE_CAPACITY);
        let context = Box::leak(Box::new(HostContext {
            inbound: StdMutex::new(Some(inbound_tx)),
            wake: Arc::new(Notify::new()),
            config: StdMutex::new(config),
            faulted: AtomicBool::new(false),
            init_failed: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            init_started: AtomicBool::new(false),
            init_notify: Notify::new(),
            ready: AtomicBool::new(false),
            broker_routing: AtomicBool::new(false),
            ready_notify: Notify::new(),
            inflight: AtomicUsize::new(0),
        }));
        let transport = Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            module: StdMutex::new(None),
            inbound: Mutex::new(inbound_rx),
            context,
            label,
            initializer: StdMutex::new(None),
            init_result: OnceCell::new(),
            pending: StdMutex::new(VecDeque::new()),
            drain_started: AtomicBool::new(false),
            lifecycle: Arc::new(Mutex::new(())),
            stop_task_spawned: Arc::new(AtomicBool::new(false)),
            initialization_gate: StdMutex::new(Some(true)),
            initialization_gate_changed: Notify::new(),
        });
        let config = context.config.lock().expect("module config lock");
        let config_slice = crate::module::abi::TbSlice {
            ptr: config.as_ptr(),
            len: config.len(),
        };
        drop(config);
        let vtable = TbHostVtable {
            size: size_of::<TbHostVtable>() as u32,
            _reserved: 0,
            host_ctx: std::ptr::from_ref(context).cast_mut().cast(),
            send: host_send,
            wake: host_wake,
            log: host_log,
            fault: host_fault,
            config: config_slice,
            ready: host_ready,
            broker_routing: Some(host_broker_routing),
        };
        (transport, vtable)
    }

    pub(crate) fn initialize(&self, module: TbModuleVtable) -> Result<()> {
        if module.size < TB_MODULE_VTABLE_BASE_SIZE
            || module.module_ctx.is_null()
            || (module.size >= size_of::<TbModuleVtable>() as u32 && module.reinitialize.is_none())
        {
            return Err(Error::transport("module returned an incomplete vtable"));
        }
        *self.module.lock().expect("module vtable lock") = Some(module);
        Ok(())
    }

    pub(crate) fn defer_initialize(
        &self,
        init: crate::module::abi::TbModuleInit,
        host: TbHostVtable,
    ) {
        self.defer_initializer(host, move |host| {
            let mut module = TbModuleVtable::default();
            let code = unsafe { init(&host, &mut module) };
            if code == TB_OK {
                Ok(module)
            } else {
                Err("module initialization failed".to_string())
            }
        });
    }

    pub(crate) fn defer_initializer<F>(&self, host: TbHostVtable, initialize: F)
    where
        F: FnOnce(TbHostVtable) -> std::result::Result<TbModuleVtable, String> + Send + 'static,
    {
        *self.initializer.lock().expect("module initializer lock") = Some(DeferredInitializer {
            initialize: Box::new(initialize),
            host,
        });
    }

    pub(crate) fn pause_initialization(&self) {
        *self
            .initialization_gate
            .lock()
            .expect("initialization gate lock") = None;
    }

    pub(crate) fn release_initialization(&self, allowed: bool) {
        *self
            .initialization_gate
            .lock()
            .expect("initialization gate lock") = Some(allowed);
        self.initialization_gate_changed.notify_waiters();
        if !allowed {
            self.terminate_waiters();
        }
    }

    // Admission rejection is a genuine fault, unlike ordinary shutdown. Never
    // fabricate READY or init_started to release a rejected observer.
    fn terminate_waiters(&self) {
        self.context.faulted.store(true, Ordering::Release);
        self.context.init_notify.notify_waiters();
        self.context.ready_notify.notify_waiters();
        self.initialization_gate_changed.notify_waiters();
    }

    // Normal completion closes admission without inventing a module fault. The
    // queue lock makes closure atomic with send admission and drainer retirement;
    // no queued body survives disposal or starts a replacement drainer.
    fn finish_stop(&self) {
        {
            let mut pending = self.pending.lock().expect("module pending lock");
            self.context.stopped.store(true, Ordering::Release);
            pending.clear();
        }
        self.context.init_notify.notify_waiters();
        self.context.ready_notify.notify_waiters();
        self.context.wake.notify_waiters();
        self.initialization_gate_changed.notify_waiters();
    }

    fn is_stopped(&self) -> bool {
        self.context.stopped.load(Ordering::Acquire)
    }

    pub(crate) async fn ensure_initialized(&self) -> Result<()> {
        loop {
            let changed = self.initialization_gate_changed.notified();
            let terminal = self.context.init_notify.notified();
            let allowed = *self
                .initialization_gate
                .lock()
                .expect("initialization gate lock");
            if self.is_faulted() || self.is_stopped() {
                return Err(Error::ConnectionClosed);
            }
            match allowed {
                Some(true) => break,
                Some(false) => return Err(Error::transport("module callback admission failed")),
                None => tokio::select! {
                    _ = changed => {},
                    _ = terminal => {},
                },
            }
        }

        let result = self
            .init_result
            .get_or_init(|| async {
                {
                    // Pair starting opaque initialization with the never-started
                    // stop decision. A stopped admission cannot start it later.
                    let _gate = self
                        .initialization_gate
                        .lock()
                        .expect("initialization gate lock");
                    if self.is_stopped() {
                        return Err(InitFailure::Closed);
                    }
                    self.context.init_started.store(true, Ordering::Release);
                }
                self.context.init_notify.notify_waiters();
                let Some(initializer) = self
                    .initializer
                    .lock()
                    .expect("module initializer lock")
                    .take()
                else {
                    return if self.module.lock().expect("module vtable lock").is_some() {
                        Ok(())
                    } else {
                        Err(InitFailure::Failed("module has no initializer".to_string()))
                    };
                };
                // Module init is opaque synchronous code. Keep it off Tokio's
                // worker threads and bound how long the broker waits; a timed
                // out blocking task may remain wedged, so its borrowed config
                // remains allocated rather than being invalidated underneath it.
                let initialized = tokio::time::timeout(
                    MODULE_INIT_DEADLINE,
                    tokio::task::spawn_blocking(move || initializer.run()),
                )
                .await;
                let module = match initialized {
                    Ok(Ok(Ok(initialized))) => initialized,
                    Ok(Ok(Err(reason))) => {
                        self.clear_config();
                        return Err(InitFailure::Failed(reason));
                    }
                    Ok(Err(_)) => {
                        return Err(InitFailure::Failed(
                            "module initialization panicked".to_string(),
                        ));
                    }
                    Err(_) => {
                        return Err(InitFailure::Failed(
                            "module initialization exceeded its deadline".to_string(),
                        ));
                    }
                };
                self.clear_config();
                self.initialize(module.0).map_err(|_| {
                    InitFailure::Failed("module returned an invalid vtable".to_string())
                })?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) if self.is_stopped() => Err(Error::ConnectionClosed),
            Ok(()) => Ok(()),
            Err(InitFailure::Closed) => Err(Error::ConnectionClosed),
            Err(InitFailure::Failed(reason)) => Err(Error::transport(reason.clone())),
        }
    }

    pub(crate) async fn wait_ready(&self) {
        loop {
            let notified = self.context.ready_notify.notified();
            if self.context.ready.load(Ordering::Acquire)
                || self.context.faulted.load(Ordering::Acquire)
                || self.is_stopped()
            {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.context.ready.load(Ordering::Acquire) && !self.is_faulted() && !self.is_stopped()
    }

    pub(crate) fn clear_config(&self) {
        let mut config = self.context.config.lock().expect("module config lock");
        config.fill(0);
        config.clear();
        config.shrink_to_fit();
    }

    pub(crate) async fn reinitialize(self: &Arc<Self>, bytes: crate::Secret) -> Result<()> {
        if self.is_stopped() {
            return Err(Error::ConnectionClosed);
        }
        let started = std::time::Instant::now();
        let lifecycle =
            tokio::time::timeout(MODULE_INIT_DEADLINE, self.lifecycle.clone().lock_owned())
                .await
                .map_err(|_| Error::Timeout {
                    member: crate::MemberName::new("ReinitializeModule")
                        .expect("static member name is valid"),
                    timeout_ms: MODULE_INIT_DEADLINE.as_millis() as u64,
                })?;
        if self.is_stopped() {
            return Err(Error::ConnectionClosed);
        }
        let module = self
            .module
            .lock()
            .expect("module vtable lock")
            .ok_or_else(|| Error::transport("module is not initialized"))?;
        if module.size < size_of::<TbModuleVtable>() as u32 {
            return Err(Error::failed("module does not support reinitialization"));
        }
        let callback = module
            .reinitialize
            .ok_or_else(|| Error::failed("module does not support reinitialization"))?;
        let module = SendModuleVtable(module);
        let remaining = MODULE_INIT_DEADLINE.saturating_sub(started.elapsed());
        let task = tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            unsafe { module.reinitialize(callback, bytes.expose_secret()) }
        });
        let code = tokio::time::timeout(remaining, task)
            .await
            .map_err(|_| Error::Timeout {
                member: crate::MemberName::new("ReinitializeModule")
                    .expect("static member name is valid"),
                timeout_ms: MODULE_INIT_DEADLINE.as_millis() as u64,
            })?
            .map_err(|_| Error::transport("module reinitialization task was cancelled"))?;
        match code {
            TB_OK => Ok(()),
            TB_BAD_ARGUMENT => Err(Error::failed("module configuration is invalid")),
            TB_CLOSED => Err(Error::failed("module reinitialization failed")),
            TB_PANICKED => Err(Error::failed("module reinitialization panicked")),
            TB_TIMEOUT => Err(Error::Timeout {
                member: crate::MemberName::new("ReinitializeModule")
                    .expect("static member name is valid"),
                timeout_ms: MODULE_INIT_DEADLINE.as_millis() as u64,
            }),
            _ => Err(Error::failed("module reinitialization failed")),
        }
    }

    pub(crate) fn is_faulted(&self) -> bool {
        self.context.faulted.load(Ordering::Acquire)
    }

    pub(crate) fn init_failed(&self) -> bool {
        self.context.init_failed.load(Ordering::Acquire)
    }

    pub(crate) fn init_started(&self) -> bool {
        self.context.init_started.load(Ordering::Acquire)
    }

    pub(crate) fn inflight(&self) -> usize {
        self.context.inflight.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_initializing(&self) {
        loop {
            let notified = self.context.init_notify.notified();
            if self.init_started() || self.is_faulted() || self.is_stopped() {
                return;
            }
            notified.await;
        }
    }

    async fn deliver_now(&self, message: Message) -> Result<()> {
        if self.is_faulted() || self.is_stopped() {
            return Err(Error::ConnectionClosed);
        }
        let bytes = serde_json::to_vec(&message)?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(Error::protocol("module frame exceeds the size cap"));
        }

        loop {
            let notified = self.context.wake.notified();
            if self.is_faulted() || self.is_stopped() {
                return Err(Error::ConnectionClosed);
            }
            let module = *self.module.lock().expect("module vtable lock");
            let Some(module) = module else {
                return Err(Error::ConnectionClosed);
            };
            let code = unsafe { (module.deliver)(module.module_ctx, bytes.as_ptr(), bytes.len()) };
            match code {
                TB_OK => return Ok(()),
                TB_BACKPRESSURE => {
                    // A module that stops draining its queue and never calls
                    // `wake` must not park this delivery task — or the caller
                    // behind it — for the process lifetime. Expiry faults the
                    // module downstream so later callers get `ModuleUnavailable`
                    // instead of waiting out their own deadline.
                    if tokio::time::timeout(BACKPRESSURE_DEADLINE, notified)
                        .await
                        .is_err()
                    {
                        let _pending = self.pending.lock().expect("module pending lock");
                        if self.is_stopped() {
                            return Err(Error::ConnectionClosed);
                        }
                        self.context.faulted.store(true, Ordering::Release);
                        self.context.ready_notify.notify_waiters();
                        self.context
                            .inbound
                            .lock()
                            .expect("host inbound lock")
                            .take();
                        return Err(Error::transport(
                            "module stopped draining its queue within the deadline",
                        ));
                    }
                }
                TB_CLOSED => return Err(Error::ConnectionClosed),
                TB_BAD_ARGUMENT => return Err(Error::protocol("module refused a valid frame")),
                _ => return Err(Error::transport("module delivery callback failed")),
            }
        }
    }

    async fn drain_pending(self: Arc<Self>) {
        self.wait_ready().await;
        loop {
            while !self.is_faulted() && !self.is_stopped() {
                let message = self
                    .pending
                    .lock()
                    .expect("module pending lock")
                    .pop_front();
                let Some(message) = message else {
                    break;
                };
                if let Err(error) = self.deliver_now(message.clone()).await {
                    self.fault_pending(message, error).await;
                    self.drain_started.store(false, Ordering::Release);
                    return;
                }
            }
            if self.is_faulted() || self.is_stopped() {
                self.drain_started.store(false, Ordering::Release);
                return;
            }
            // A `send` that pushed while the loop observed an empty queue saw
            // `drain_started == true` and skipped spawning its own drainer, so
            // it is this loop's job to pick the message up. Clear the flag only
            // while holding the queue lock, and return only once the queue is
            // genuinely empty; the lock makes the check-and-clear atomic with a
            // concurrent push.
            let restart = {
                let pending = self.pending.lock().expect("module pending lock");
                let empty = pending.is_empty();
                if empty {
                    self.drain_started.store(false, Ordering::Release);
                }
                !empty
            };
            if !restart {
                return;
            }
            // Fall back into the drain loop for the newly queued messages.
        }
    }

    /// Answer every still-queued call — and the one that just failed to
    /// deliver — with `ModuleUnavailable`, then fault the module, so callers
    /// learn the delivery failed instead of waiting out their deadline.
    async fn fault_pending(&self, failed: Message, error: Error) {
        let cleared = {
            let mut pending = self.pending.lock().expect("module pending lock");
            if self.is_stopped() && matches!(error, Error::ConnectionClosed) {
                pending.clear();
                return;
            }
            self.context.faulted.store(true, Ordering::Release);
            std::mem::take(&mut *pending)
        };
        // Each of these calls incremented `inflight` when it was queued. The
        // synthesized error replies below travel back through `recv`, which
        // refunds one slot per reply, so no manual adjustment is needed.
        let sender = self
            .context
            .inbound
            .lock()
            .expect("host inbound lock")
            .clone();
        if let Some(sender) = sender {
            for queued in cleared.iter().chain([&failed]) {
                if queued.header.kind == MessageKind::MethodCall {
                    let error = Error::ModuleUnavailable {
                        module: self.label.clone(),
                        state: "faulted".to_string(),
                        detail: "module stopped accepting calls".to_string(),
                    };
                    let reply = Message::error_reply(&queued.header, &error);
                    if let Ok(bytes) = serde_json::to_vec(&reply) {
                        let _ = sender.try_send(bytes);
                    }
                }
            }
        }
        self.context.faulted.store(true, Ordering::Release);
        self.context.ready_notify.notify_waiters();
        self.context
            .inbound
            .lock()
            .expect("host inbound lock")
            .take();
    }

    pub(crate) fn shutdown_sync(&self, deadline: Duration) -> i32 {
        let module = *self.module.lock().expect("module vtable lock");
        match module {
            Some(module) => unsafe {
                (module.shutdown)(module.module_ctx, deadline.as_millis() as u64)
            },
            None => TB_CLOSED,
        }
    }

    pub(crate) fn stop_sync(&self, deadline: Duration) -> i32 {
        if self.is_stopped() {
            return TB_CLOSED;
        }
        {
            let _gate = self
                .initialization_gate
                .lock()
                .expect("initialization gate lock");
            if !self.init_started() && self.module.lock().expect("module vtable lock").is_none() {
                self.finish_stop();
            }
        }
        let code = self.shutdown_sync(deadline);
        // A caller timeout is not physical completion. Running initialization
        // and readiness observers are closed only after the opaque callback returns.
        self.finish_stop();
        self.context
            .inbound
            .lock()
            .expect("host inbound lock")
            .take();
        code
    }

    pub(crate) fn stop_task_spawned(&self) -> bool {
        self.stop_task_spawned.load(Ordering::Acquire)
    }

    pub(crate) async fn stop(
        self: Arc<Self>,
        deadline: Duration,
    ) -> std::result::Result<i32, StopError> {
        if self.is_stopped() {
            return Err(StopError::NotStarted(Error::ConnectionClosed));
        }
        let started = std::time::Instant::now();
        let lifecycle = tokio::time::timeout(deadline, self.lifecycle.clone().lock_owned())
            .await
            .map_err(|_| {
                StopError::NotStarted(Error::failed(
                    "module lifecycle operation exceeded its deadline",
                ))
            })?;
        if self.is_stopped() {
            return Err(StopError::NotStarted(Error::ConnectionClosed));
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        // A retry that cannot acquire the physical lock must not erase the
        // original running stop's host transition. Only its new lock owner may
        // replace that marker, immediately before handing the lock to the task.
        self.stop_task_spawned.store(true, Ordering::Release);
        let task = tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            self.stop_sync(remaining)
        });
        tokio::time::timeout(remaining, task)
            .await
            .map_err(|_| {
                StopError::Started(Error::failed("module shutdown exceeded its deadline"))
            })?
            .map_err(|_| StopError::Started(Error::failed("module shutdown task was cancelled")))
    }
}

#[derive(Debug)]
pub(crate) enum StopError {
    NotStarted(Error),
    Started(Error),
}

impl From<StopError> for Error {
    fn from(error: StopError) -> Self {
        match error {
            StopError::NotStarted(error) | StopError::Started(error) => error,
        }
    }
}

#[async_trait]
impl Transport for ModuleTransport {
    async fn send(&self, message: Message) -> Result<()> {
        if self.is_stopped() {
            return Err(Error::ConnectionClosed);
        }
        if let Err(error) = self.ensure_initialized().await {
            {
                let _pending = self.pending.lock().expect("module pending lock");
                if self.is_stopped() && matches!(error, Error::ConnectionClosed) {
                    return Err(Error::ConnectionClosed);
                }
                self.context.init_failed.store(true, Ordering::Release);
                self.context.faulted.store(true, Ordering::Release);
            }
            self.context.ready_notify.notify_waiters();
            if message.header.kind == MessageKind::MethodCall {
                let error = Error::ModuleUnavailable {
                    module: self.label.clone(),
                    state: "failed".to_string(),
                    detail: "module initialization failed".to_string(),
                };
                let reply = Message::error_reply(&message.header, &error);
                if let Ok(bytes) = serde_json::to_vec(&reply) {
                    let sender = self
                        .context
                        .inbound
                        .lock()
                        .expect("host inbound lock")
                        .take();
                    if let Some(sender) = sender {
                        let _ = sender.try_send(bytes);
                    }
                }
                return Ok(());
            }
            return Err(Error::ConnectionClosed);
        }
        let is_call = message.header.kind == MessageKind::MethodCall;
        {
            let mut pending = self.pending.lock().expect("module pending lock");
            if self.is_stopped() {
                return Err(Error::ConnectionClosed);
            }
            if is_call {
                self.context.inflight.fetch_add(1, Ordering::AcqRel);
            }
            if is_call && !self.is_ready() {
                pending.push_back(message);
                if !self.drain_started.swap(true, Ordering::AcqRel) {
                    let transport = self.self_ref.upgrade().ok_or(Error::ConnectionClosed)?;
                    tokio::spawn(transport.drain_pending());
                }
                return Ok(());
            }
        }
        let result = self.deliver_now(message).await;
        if result.is_err() && is_call {
            self.context.inflight.fetch_sub(1, Ordering::AcqRel);
        }
        result
    }

    async fn recv(&self) -> Result<Option<Message>> {
        let bytes = self.inbound.lock().await.recv().await;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let message: Message = serde_json::from_slice(&bytes)?;
        if matches!(
            message.header.kind,
            MessageKind::MethodReturn | MessageKind::Error
        ) {
            // Saturating decrement via a CAS loop (avoids `fetch_update`, which
            // newer toolchains deprecate in favour of `try_update`).
            let inflight = &self.context.inflight;
            let mut current = inflight.load(Ordering::Acquire);
            while let Some(next) = current.checked_sub(1) {
                match inflight.compare_exchange_weak(
                    current,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
        }
        Ok(Some(message))
    }

    async fn close(&self) -> Result<()> {
        // The module's `shutdown` callback may block up to its deadline. Run it
        // on a blocking thread so a wedged shutdown cannot stall the broker
        // task that is closing this transport.
        let transport = self.self_ref.upgrade().ok_or(Error::ConnectionClosed)?;
        transport.stop(Duration::from_secs(5)).await?;
        Ok(())
    }

    fn describe(&self) -> String {
        format!("module:{}", self.label)
    }
}

unsafe extern "C" fn host_send(ctx: *mut c_void, ptr: *const u8, len: usize) -> i32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if ctx.is_null() || ptr.is_null() || len > MAX_FRAME_LEN {
            return TB_BAD_ARGUMENT;
        }
        let context = unsafe { &*(ctx.cast::<HostContext>()) };
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        let sender = context.inbound.lock().expect("host inbound lock").clone();
        match sender {
            // This callback runs on a module-owned runtime thread. Blocking it
            // applies bounded backpressure only to that module and gives the
            // async SDK a reliable completion without adding a fifth callback
            // to the frozen v1 ABI.
            Some(sender) => match if tokio::runtime::Handle::try_current().is_ok() {
                // Linked modules share Tokio's thread-local runtime state with
                // the host. `blocking_send` directly on their worker panics.
                tokio::task::block_in_place(|| sender.blocking_send(bytes))
            } else {
                sender.blocking_send(bytes)
            } {
                Ok(()) => TB_OK,
                Err(_) => TB_CLOSED,
            },
            None => TB_CLOSED,
        }
    }))
    .unwrap_or(TB_BACKPRESSURE)
}

unsafe extern "C" fn host_wake(ctx: *mut c_void) {
    // A panic escaping a plain `extern "C"` frame aborts the process, so each
    // host callback that a module can reach must contain its own unwinds.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(context) = unsafe { ctx.cast::<HostContext>().as_ref() } {
            context.wake.notify_one();
        }
    }));
}

unsafe extern "C" fn host_log(ctx: *mut c_void, level: u32, ptr: *const u8, len: usize) {
    // `host_log` is the most exposed callback: the `tracing` macros run
    // arbitrary subscriber code chosen by the embedding host, so a panicking
    // subscriber must not be able to abort the process through the module.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if ctx.is_null() || ptr.is_null() {
            return;
        }
        let len = len.min(4096);
        let message = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) });
        match level {
            1 => tracing::error!(target: "tinybus_module", message = %message),
            2 => tracing::warn!(target: "tinybus_module", message = %message),
            3 => tracing::info!(target: "tinybus_module", message = %message),
            4 => tracing::debug!(target: "tinybus_module", message = %message),
            _ => tracing::trace!(target: "tinybus_module", message = %message),
        }
    }));
}

unsafe extern "C" fn host_fault(ctx: *mut c_void, _: *const u8, _: usize) {
    // `host_fault` exists so a misbehaving module is detached instead of
    // taking the host down; an abort inside it would defeat that goal.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(context) = unsafe { ctx.cast::<HostContext>().as_ref() } {
            context.faulted.store(true, Ordering::Release);
            context.init_notify.notify_waiters();
            context.ready_notify.notify_waiters();
            context.inbound.lock().expect("host inbound lock").take();
        }
    }));
}

unsafe extern "C" fn host_broker_routing(ctx: *mut c_void) -> u32 {
    // HostContext is leaked for the lifetime of the vtable; no module-owned
    // JSON or transport label can grant this provenance.
    unsafe { ctx.cast::<HostContext>().as_ref() }
        .is_some_and(|context| context.broker_routing.load(Ordering::Acquire)) as u32
}

unsafe extern "C" fn host_ready(ctx: *mut c_void) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(context) = unsafe { ctx.cast::<HostContext>().as_ref() } {
            context.ready.store(true, Ordering::Release);
            context.ready_notify.notify_waiters();
        }
    }));
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
