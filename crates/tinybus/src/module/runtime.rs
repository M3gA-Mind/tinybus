//! The module runtime: a broker, a connection and a loader on a dedicated thread.
//!
//! # Why a runtime of its own
//!
//! A host's event bus usually constructs its broker internally and never hands
//! it out, while [`ModuleHost::new`] needs a [`Broker`] to attach loaded modules
//! to. Modules therefore run on a second in-process broker. A module on it can
//! serve requests but cannot publish into the host's own event flow.
//!
//! # What loading a module means
//!
//! `dlopen` runs code before anything can inspect it. The ABI descriptor,
//! manifest and digest gates decide whether a module is *admitted*, not whether
//! it is *safe*: once loaded it shares the process's address space, and a native
//! fault in it takes the host down. A library is never unloaded, so a module that
//! fails is failed until restart; [`super::Resolver`] caches failures for that
//! reason instead of retrying.
//!
//! The broker lives on a dedicated, process-lifetime Tokio runtime. A module
//! therefore stays usable across short-lived caller runtimes (notably
//! `#[tokio::test]`), and an embedder cannot tie its lifetime to an independently
//! managed application runtime.
//!
//! Admission is the default permissive mode. Strict mode additionally refuses a
//! module whose `rustc` version string differs from the host's, which refuses
//! real CI-built artifacts outright. The ABI revision, descriptor layout, target
//! triple, pointer width, endianness, feature bits and the refusal of a
//! `panic=abort` module are all still enforced.

use std::future::Future;
use std::pin::Pin;

use crate::broker::Broker;
use crate::module::ModuleHost;
use crate::transport::memory::MemoryBus;
use crate::{Connection, Proxy};

/// Work run once on the new runtime, before the first module can load.
///
/// A host uses it to install the callback services its modules call back into,
/// so they are being served before any module resolves.
pub type RuntimeSetup =
    Box<dyn FnOnce(Connection) -> Pin<Box<dyn Future<Output = crate::Result<()>> + Send>> + Send>;

/// Thread names for a [`ModuleRuntime`], so a host's threads are recognisable.
#[derive(Debug, Clone)]
pub struct RuntimeNames {
    /// Name of the thread that owns the Tokio runtime.
    pub thread: String,
    /// Name of the runtime's worker threads.
    pub worker: String,
}

impl Default for RuntimeNames {
    fn default() -> Self {
        Self {
            thread: "tinybus-module-bus".to_string(),
            worker: "tinybus-module-worker".to_string(),
        }
    }
}

/// The broker, the host's own connection to it, and the module loader.
pub struct ModuleRuntime {
    /// The loader. Owns every admitted module for the process lifetime.
    host: ModuleHost,
    /// This process's client connection, used to call into loaded modules.
    connection: Connection,
    /// Handle for work that must outlive the caller's runtime.
    handle: tokio::runtime::Handle,
}

impl ModuleRuntime {
    /// Start a runtime on its own thread and wait until it is ready.
    ///
    /// The thread, the broker and the Tokio runtime live for the process: there
    /// is no shutdown path because there is nothing a shutdown could reclaim.
    /// Call it once and keep the result in a `static`.
    ///
    /// # Errors
    ///
    /// Returns an error if the thread or Tokio runtime cannot be built, the
    /// in-memory transport cannot be connected, or `setup` fails.
    pub fn start(names: &RuntimeNames, setup: Option<RuntimeSetup>) -> crate::Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker = names.worker.clone();
        std::thread::Builder::new()
            .name(names.thread.clone())
            .spawn(move || {
                let tokio_runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name(worker)
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(crate::Error::Transport(error.to_string())));
                        return;
                    }
                };
                match tokio_runtime.block_on(build(setup)) {
                    Ok(runtime) => {
                        let _ = ready_tx.send(Ok(runtime));
                        tokio_runtime.block_on(std::future::pending::<()>());
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|error| crate::Error::Transport(error.to_string()))?;
        ready_rx
            .recv()
            .map_err(|error| crate::Error::Transport(error.to_string()))?
    }

    /// The loader.
    #[must_use]
    pub fn host(&self) -> &ModuleHost {
        &self.host
    }

    /// The connection modules are called over.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Spawn `future` with process lifetime.
    ///
    /// For work that other callers wait on, a module resolution, and that must
    /// therefore outlive whichever caller runtime started it.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    /// Run module admission work on this runtime's blocking pool.
    ///
    /// # Errors
    ///
    /// The work's own error, or a message if the loader did not finish.
    pub async fn blocking<F>(&self, work: F) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        self.handle
            .spawn_blocking(work)
            .await
            .map_err(|error| format!("the module loader did not finish: {error}"))?
    }

    /// A proxy for one object on a loaded module.
    ///
    /// # Errors
    ///
    /// Returns an error if `bus_name` or `object_path` is not well formed, which
    /// for a registry entry means the table is wrong rather than the module.
    pub fn proxy(&self, bus_name: &str, object_path: &str) -> crate::Result<Proxy> {
        self.connection.proxy(bus_name, object_path, bus_name)
    }
}

async fn build(setup: Option<RuntimeSetup>) -> crate::Result<ModuleRuntime> {
    let transport = MemoryBus::new();
    let broker = Broker::new();
    // The broker task is deliberately not retained: it lives as long as the
    // process, and an abort handle must never be called, because a module whose
    // transport disappears faults and cannot be reloaded without a restart.
    broker.spawn(transport.clone());

    let host = ModuleHost::new(broker);
    let connection = Connection::connect(transport.connect().await?).await?;
    if let Some(setup) = setup {
        setup(connection.clone()).await?;
    }
    Ok(ModuleRuntime {
        host,
        connection,
        handle: tokio::runtime::Handle::current(),
    })
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
