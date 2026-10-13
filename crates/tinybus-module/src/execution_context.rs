//! Per-instance SDK execution context; no module-global host callback owner.
use super::*;
use std::cell::RefCell;

thread_local! {
    static OWNER: RefCell<Option<ExecutionOwner>> = const { RefCell::new(None) };
    static WORKER: RefCell<Option<ExecutionGuard>> = const { RefCell::new(None) };
}
static PANIC_HOOK: std::sync::Once = std::sync::Once::new();

#[derive(Clone)]
pub(super) struct ExecutionOwner {
    host: HostCalls,
    dispatch: tracing::Dispatch,
    pub(super) panic_location: Arc<StdMutex<Option<String>>>,
    live: Arc<AtomicBool>,
}
impl ExecutionOwner {
    pub(super) fn new(host: HostCalls) -> Self {
        let live = Arc::new(AtomicBool::new(true));
        Self {
            host,
            dispatch: tracing::Dispatch::new(LiveSubscriber {
                inner: HostSubscriber {
                    host,
                    next_span: AtomicU64::new(1),
                    max_level: tracing::level_filters::LevelFilter::TRACE,
                },
                live: live.clone(),
            }),
            panic_location: Arc::new(StdMutex::new(None)),
            live,
        }
    }
    #[cfg(feature = "__native-fixture-support")]
    pub(super) fn is_current_for_fixture(&self) -> bool {
        OWNER.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(&self.live, &current.live))
        })
    }
    #[cfg(feature = "__native-fixture-support")]
    pub(super) fn is_stopped_for_fixture(&self) -> bool {
        !self.live.load(Ordering::Acquire)
    }
    pub(super) fn enter(&self) -> ExecutionGuard {
        let previous = OWNER.with(|slot| slot.replace(Some(self.clone())));
        let dispatch = tracing::dispatcher::set_default(&self.dispatch);
        ExecutionGuard {
            dispatch: Some(dispatch),
            previous,
        }
    }
    pub(super) fn stop(&self) {
        self.live.store(false, Ordering::Release);
    }
    pub(super) fn configure(&self, builder: &mut tokio::runtime::Builder) {
        let owner = self.clone();
        builder
            .on_thread_start(move || WORKER.with(|slot| *slot.borrow_mut() = Some(owner.enter())));
        builder.on_thread_stop(|| {
            WORKER.with(|slot| {
                slot.borrow_mut().take();
            })
        });
    }
    pub(super) fn install_panic_hook() {
        PANIC_HOOK.call_once(|| {
            std::panic::set_hook(Box::new(move |panic| {
                let owner = OWNER
                    .try_with(|slot| slot.try_borrow().ok().and_then(|owner| owner.clone()))
                    .ok()
                    .flatten();
                let location = panic.location().map_or_else(
                    || "module panicked at an unknown location".to_string(),
                    |location| {
                        let file = std::path::Path::new(location.file())
                            .file_name()
                            .and_then(|file| file.to_str())
                            .unwrap_or("module");
                        format!(
                            "module panicked at {}:{}:{}",
                            file,
                            location.line(),
                            location.column()
                        )
                    },
                );
                let Some(owner) = owner else {
                    // Unowned module-created threads have no callback authority.
                    // Preserve payload sanitization without selecting a host.
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr().lock(), "{location}");
                    return;
                };
                if !owner.live.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(mut slot) = owner.panic_location.try_lock() {
                    *slot = Some(location.clone());
                }
                owner.host.log(1, location.as_bytes());
            }));
        });
    }
}
pub(super) struct ExecutionGuard {
    dispatch: Option<tracing::dispatcher::DefaultGuard>,
    previous: Option<ExecutionOwner>,
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.dispatch.take();
        let previous = self.previous.take();
        let _ = OWNER.try_with(|slot| {
            if let Ok(mut owner) = slot.try_borrow_mut() {
                *owner = previous;
            }
        });
    }
}
struct LiveSubscriber {
    inner: HostSubscriber,
    live: Arc<AtomicBool>,
}
impl tracing::Subscriber for LiveSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        self.live.load(Ordering::Acquire) && self.inner.enabled(metadata)
    }
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        self.inner.max_level_hint()
    }
    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        self.inner.new_span(attrs)
    }
    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        self.inner.record(span, values);
    }
    fn record_follows_from(&self, span: &tracing::span::Id, follows: &tracing::span::Id) {
        self.inner.record_follows_from(span, follows);
    }
    fn event(&self, event: &tracing::Event<'_>) {
        if self.live.load(Ordering::Acquire) {
            self.inner.event(event);
        }
    }
    fn enter(&self, span: &tracing::span::Id) {
        self.inner.enter(span);
    }
    fn exit(&self, span: &tracing::span::Id) {
        self.inner.exit(span);
    }
}

#[cfg(test)]
#[path = "execution_context_tests.rs"]
mod tests;
