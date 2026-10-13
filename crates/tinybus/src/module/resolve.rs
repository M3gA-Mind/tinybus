//! Pure manifest dependency resolution and deterministic load ordering, and the
//! [`Resolver`] that turns a pinned release into a serving module.
//!
//! # The ladder
//!
//! [`Resolver::ensure_loaded`] resolves a module once per process and remembers
//! the outcome, including failure. A library is never unloaded, so a module that
//! was refused, faulted or failed to initialise keeps what it mapped and loading
//! it again cannot reach a different outcome. Retrying would pay a download and
//! a `dlopen` on every call to arrive at the same error, so a failure is cached
//! and returned directly; fixing a module means restarting the process.
//!
//! Resolution order is cheapest first: already loaded, an override, the module
//! search path, then the release cache (verified against the host's pinned
//! digest, downloaded on a miss when allowed), then pruning of stale versions.
//!
//! Each module has one slot in [`super::resolution`]. The first caller runs the
//! resolution as a process-lifetime task and every other caller waits on its
//! outcome; two modules never queue behind each other.
//! [`Resolver::ensure_loaded_within`] bounds the wait, never the resolution.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::module::failure::{FailureReporter, Reason};
use crate::module::manifest::ModuleManifest;
use crate::module::platform::host_candidates;
use crate::module::resolution::{self, Claim, Resolution, ResolutionState, Waited};
use crate::module::runtime::ModuleRuntime;
use crate::module::{ReleaseAsset, ReleasePlan, load_first_admitted, prune_stale_versions};

/// Result indexes refer to the input manifest slice.
pub(crate) struct LoadOrder {
    pub(crate) order: Vec<usize>,
    pub(crate) unresolved: Vec<(usize, String)>,
}

/// Reject collisions, ignore absent optional dependencies, and topologically
/// order every remaining manifest.
pub(crate) fn resolve(
    manifests: &[ModuleManifest],
    initially_available: &HashSet<String>,
) -> LoadOrder {
    let duplicate_modules = duplicates(manifests.iter().map(|manifest| &manifest.module.name));
    let duplicate_bus_names = duplicates(manifests.iter().map(|manifest| &manifest.bus_name));
    let mut unresolved = Vec::new();
    let mut pending = Vec::new();
    for (index, manifest) in manifests.iter().enumerate() {
        if duplicate_modules.contains(&manifest.module.name) {
            unresolved.push((
                index,
                "two artifacts declare the same module name".to_string(),
            ));
        } else if duplicate_bus_names.contains(&manifest.bus_name) {
            unresolved.push((index, "two modules claim the same bus name".to_string()));
        } else {
            pending.push(index);
        }
    }

    let mut available = initially_available.clone();
    let mut order = Vec::new();
    while !pending.is_empty() {
        let ready = pending.iter().position(|index| {
            manifests[*index]
                .requires
                .iter()
                .filter(|dependency| !dependency.optional)
                .all(|dependency| available.contains(dependency.interface.interface.as_str()))
        });
        if let Some(position) = ready {
            let index = pending.remove(position);
            available.extend(
                manifests[index]
                    .provides
                    .iter()
                    .map(|provided| provided.version.interface.to_string()),
            );
            order.push(index);
            continue;
        }

        // Remove genuinely blocked providers first. Their interfaces then stop
        // masking missing dependencies in consumers behind them; only a fixed
        // point with no absent provider is a real cycle.
        let declared = pending
            .iter()
            .flat_map(|index| manifests[*index].provides.iter())
            .map(|provided| provided.version.interface.to_string())
            .collect::<HashSet<_>>();
        let blocked = pending.iter().position(|index| {
            manifests[*index]
                .requires
                .iter()
                .filter(|dependency| !dependency.optional)
                .any(|dependency| {
                    !available.contains(dependency.interface.interface.as_str())
                        && !declared.contains(dependency.interface.interface.as_str())
                })
        });
        if let Some(position) = blocked {
            let index = pending.remove(position);
            let missing = manifests[index]
                .requires
                .iter()
                .filter(|dependency| !dependency.optional)
                .find(|dependency| {
                    !available.contains(dependency.interface.interface.as_str())
                        && !declared.contains(dependency.interface.interface.as_str())
                })
                .expect("blocked module has a missing dependency");
            unresolved.push((
                index,
                format!(
                    "required interface {} has no provider",
                    missing.interface.interface
                ),
            ));
            continue;
        }
        for index in pending.drain(..) {
            unresolved.push((index, "module dependency cycle detected".to_string()));
        }
    }
    LoadOrder { order, unresolved }
}

fn duplicates<T: Eq + std::hash::Hash>(values: impl Iterator<Item = T>) -> HashSet<T> {
    let mut counts = HashMap::new();
    for value in values {
        *counts.entry(value).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(value, count)| (count > 1).then_some(value))
        .collect()
}

/// A first-party module a host knows how to load.
///
/// Every field is supplied by the host's compiled registry rather than
/// discovered: which modules exist, which interface they claim and which bytes
/// are legitimate are build decisions, not whatever a release page serves today.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseRecord {
    /// Stable identifier used by config, RPC and `ensure_loaded`.
    pub id: &'static str,
    /// Well-known bus name the module claims once it is serving.
    pub bus_name: &'static str,
    /// Object path the module serves its interface at.
    pub object_path: &'static str,
    /// Release version, matching the tag the assets come from.
    pub version: &'static str,
    /// GitHub release tag URL the artifacts are published under.
    pub release_url: &'static str,
    /// Per-host artifacts and the digests that make them legitimate.
    pub assets: &'static [ReleaseAsset<'static>],
}

impl ReleaseRecord {
    /// The asset for `host_key`, if this release publishes one.
    #[must_use]
    pub fn asset_for(&self, host_key: &str) -> Option<&'static ReleaseAsset<'static>> {
        self.assets.iter().find(|asset| asset.host_key == host_key)
    }

    /// Whether this release publishes an artifact any candidate key of this
    /// host can run.
    #[must_use]
    pub fn supports_this_host(&self) -> bool {
        host_candidates()
            .iter()
            .any(|key| self.asset_for(key).is_some())
    }
}

/// Where a module's artifact should come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModuleSource {
    /// The registry-pinned GitHub release for this build.
    Pinned,
    /// A local artifact, for developing a module against a live host.
    LocalFile {
        /// Absolute path to the platform library.
        path: String,
    },
}

/// A module's state as a host reports it.
///
/// Coarser than [`super::ModuleState`]: a caller wants to know whether it can
/// use the module, not which admission step it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Known to this build, not loaded yet.
    Available,
    /// Being downloaded, verified or initialised.
    Loading,
    /// Serving its interface.
    Ready,
    /// Cannot be loaded in this process, and will not be retried.
    Failed,
    /// No artifact is published for this host, or modules are disabled.
    Unsupported,
}

/// Why a bounded [`Resolver::ensure_loaded_within`] did not end with the module
/// serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// Terminal for the process; a message fit for a user or a model.
    Failed(String),
    /// Still being downloaded, verified or initialised when the bound passed.
    /// Asking again later can succeed.
    StillLoading,
}

impl LoadError {
    /// The message for a caller that cannot distinguish the two cases.
    #[must_use]
    pub fn into_message(self) -> String {
        match self {
            Self::Failed(message) => message,
            Self::StillLoading => "the module is still loading; try again shortly".to_string(),
        }
    }
}

/// Future returned by a [`RuntimeProvider`].
pub type RuntimeFuture = Pin<Box<dyn Future<Output = crate::Result<&'static ModuleRuntime>> + Send>>;

/// Starts (or returns) the host's process-wide [`ModuleRuntime`], lazily, so a
/// refusal or a cached outcome never starts a broker.
pub type RuntimeProvider = Arc<dyn Fn() -> RuntimeFuture + Send + Sync>;

/// Builds the configuration blob handed to a module when it is initialised.
pub type ModuleConfigProvider = Arc<dyn Fn(&str) -> serde_json::Value + Send + Sync>;

/// Plain inputs a host builds a [`Resolver`] from.
pub struct ResolverInputs {
    /// Whether modules are enabled at all.
    pub enabled: bool,
    /// Where the release cache lives. `None` means nothing can be installed.
    pub install_dir: Option<PathBuf>,
    /// Whether a release-cache miss may reach the network.
    pub allow_download: bool,
    /// A read-only installer-shipped cache root, consulted first.
    pub bundled_dir: Option<PathBuf>,
    /// Per-module artifact sources. A module absent here, or `Pinned`, loads the
    /// pinned release.
    pub sources: HashMap<String, ModuleSource>,
    /// The compiled registry.
    pub records: Vec<&'static ReleaseRecord>,
    /// Sentence appended to every terminal loader error, e.g. "This is terminal
    /// for the running process". Empty appends nothing the second time.
    pub terminal_marker: String,
    /// Starts the module runtime.
    pub runtime: RuntimeProvider,
    /// Per-module initialisation configuration.
    pub module_config: ModuleConfigProvider,
    /// Terminal-outcome reporting. Share one across the process.
    pub reporter: Arc<FailureReporter>,
}

/// Resolves registry records into serving modules. Cheap to clone.
#[derive(Clone)]
pub struct Resolver {
    inner: Arc<ResolverInputs>,
}

impl Resolver {
    /// A resolver over `inputs`.
    #[must_use]
    pub fn new(inputs: ResolverInputs) -> Self {
        Self {
            inner: Arc::new(inputs),
        }
    }

    /// The record for `id`, if the registry has one.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&'static ReleaseRecord> {
        self.inner.records.iter().copied().find(|r| r.id == id)
    }

    /// Whether modules are enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    /// The reporter terminal outcomes go through.
    #[must_use]
    pub fn reporter(&self) -> &FailureReporter {
        &self.inner.reporter
    }

    /// The module runtime, started on first use.
    ///
    /// # Errors
    ///
    /// Whatever the host's provider returns when the runtime cannot start.
    pub async fn runtime(&self) -> crate::Result<&'static ModuleRuntime> {
        (self.inner.runtime)().await
    }

    /// Report a terminal outcome for `record`, once per process.
    pub fn report(&self, record: &ReleaseRecord, reason: Reason) {
        self.inner.reporter.report(record.id, record.version, reason);
    }

    /// Report a failure of one invocation of `record`.
    pub fn report_invocation(&self, record: &ReleaseRecord, reason: Reason) {
        self.inner
            .reporter
            .report_invocation(record.id, record.version, reason);
    }

    /// Ensure `id` is loaded and serving, loading it if this is the first ask.
    ///
    /// Waits without bound; prefer [`Self::ensure_loaded_within`] from any path
    /// that has a deadline of its own.
    ///
    /// # Errors
    ///
    /// A message fit for a user or a model when the module cannot be loaded.
    pub async fn ensure_loaded(&self, id: &str) -> Result<(), String> {
        self.ensure_loaded_within(id, None)
            .await
            .map_err(LoadError::into_message)
    }

    /// [`Self::ensure_loaded`] with a bound on how long this caller waits.
    ///
    /// The bound is on the *wait*, never on the resolution: a caller that gives
    /// up leaves the download running and the slot intact, and a later call
    /// finds the outcome.
    ///
    /// # Errors
    ///
    /// [`LoadError::Failed`] when the module cannot be loaded in this process;
    /// [`LoadError::StillLoading`] when `within` passed first.
    pub async fn ensure_loaded_within(
        &self,
        id: &str,
        within: Option<Duration>,
    ) -> Result<(), LoadError> {
        if !self.inner.enabled {
            if let Some(record) = self.find(id) {
                self.report(record, Reason::Disabled);
            }
            return Err(LoadError::Failed(format!(
                "module '{id}' is unavailable: modules are disabled in configuration"
            )));
        }
        let record = self
            .find(id)
            .ok_or_else(|| LoadError::Failed(format!("unknown module '{id}'")))?;

        let receiver = match resolution::global().claim(id) {
            Claim::Done(Resolution::Ready) => return Ok(()),
            Claim::Done(Resolution::Failed(reason)) => {
                // The original resolution may have completed before a reporting
                // client was bound. Revisit reporting at the cached caller; the
                // reporter's key suppresses duplicates once delivered.
                self.report(record, Reason::ResolutionFailed);
                return Err(LoadError::Failed(reason));
            }
            Claim::Wait(receiver) => receiver,
            Claim::Run { sender, receiver } => {
                self.start_resolution(record, sender).await;
                receiver
            }
        };
        match resolution::global().wait(id, receiver, within).await {
            Waited::Ready => Ok(()),
            Waited::Failed(reason) => {
                self.report(record, Reason::ResolutionFailed);
                Err(LoadError::Failed(reason))
            }
            Waited::StillLoading => Err(LoadError::StillLoading),
        }
    }

    /// The state of `id` as the resolution table reports it, without touching it.
    #[must_use]
    pub fn state_of(&self, id: &str) -> Availability {
        match resolution::global().peek(id) {
            ResolutionState::Unresolved => Availability::Available,
            ResolutionState::Loading => Availability::Loading,
            ResolutionState::Ready => Availability::Ready,
            ResolutionState::Failed(_) => Availability::Failed,
        }
    }

    /// The reported state of `record` and, for a failed or unsupported module,
    /// why. Configuration is authoritative: a module that stays loaded after a
    /// request is still unusable to a caller whose configuration disables
    /// modules.
    #[must_use]
    pub fn status_of(&self, record: &ReleaseRecord) -> (Availability, Option<String>) {
        if !self.inner.enabled {
            return (
                Availability::Unsupported,
                Some("modules are disabled in configuration".to_string()),
            );
        }
        match resolution::global().peek(record.id) {
            ResolutionState::Ready => (Availability::Ready, None),
            ResolutionState::Loading => (Availability::Loading, None),
            ResolutionState::Failed(reason) => (Availability::Failed, Some(reason)),
            ResolutionState::Unresolved if record.supports_this_host() => {
                (Availability::Available, None)
            }
            ResolutionState::Unresolved => (
                Availability::Unsupported,
                Some("no artifact is published for this platform".to_string()),
            ),
        }
    }

    /// Run the resolution for `record` as a process-lifetime task and record its
    /// outcome. A caller runtime that shuts down mid-load must not cancel a
    /// resolution other callers are waiting on. Without a module runtime there is
    /// nothing to load into, so the outcome is recorded inline instead.
    async fn start_resolution(
        &self,
        record: &'static ReleaseRecord,
        sender: tokio::sync::watch::Sender<Option<Resolution>>,
    ) {
        let this = self.clone();
        let id = record.id;
        let work = async move {
            tracing::info!(module = id, version = record.version, "[tinybus::module] resolving");
            let resolution = match this.ladder(record).await {
                Ok(()) => {
                    tracing::info!(module = id, "[tinybus::module] serving");
                    Resolution::Ready
                }
                Err(reason) => {
                    // Every failure is terminal for the process (the outcome is
                    // cached), whichever rung produced it.
                    let reason = this.mark_terminal(reason);
                    this.report(record, Reason::ResolutionFailed);
                    Resolution::Failed(reason)
                }
            };
            resolution::global().complete(id, resolution, sender);
        };
        match self.runtime().await {
            Ok(runtime) => {
                runtime.spawn(work);
            }
            Err(error) => {
                tracing::warn!(error = %error, "[tinybus::module] the module bus could not start");
                work.await;
            }
        }
    }

    /// Get `record` serving: loaded, override, search path, release cache, prune.
    async fn ladder(&self, record: &'static ReleaseRecord) -> Result<(), String> {
        let runtime = self.runtime().await.map_err(|_| {
            format!(
                "module '{}' is unavailable: the module bus could not start",
                record.id
            )
        })?;

        // Already serving: loaded from the search path at boot, or by an earlier
        // explicit local load.
        if runtime
            .host()
            .list()
            .iter()
            .any(|info| info.manifest.bus_name.as_str() == record.bus_name)
        {
            return Ok(());
        }

        // An override points at a developer's own build, checked before the
        // pinned release so a module can be iterated on against a live host.
        if let Some(ModuleSource::LocalFile { path }) = self.inner.sources.get(record.id) {
            let path = PathBuf::from(path);
            let module_config = (self.inner.module_config)(record.id);
            return self
                .blocking(move || Self::load_local(runtime, &path, record.id, module_config))
                .await;
        }

        // The search path tinybus itself honours. A refused search-path artifact
        // is ordinary (most directories hold nothing), so errors are dropped and
        // only a match on the bus name counts.
        let bus_name = record.bus_name;
        let found_on_search_path = tokio::task::spawn_blocking(move || {
            runtime
                .host()
                .load_search_paths()
                .into_iter()
                .flatten()
                .any(|info| info.manifest.bus_name.as_str() == bus_name)
        })
        .await
        .unwrap_or(false);
        if found_on_search_path {
            return Ok(());
        }

        // The release cache: a verified artifact from an earlier launch, or a
        // download into the same place. Off the runtime worker, because the cold
        // path fetches, hashes, extracts and `dlopen`s synchronously.
        let Some(root) = self.inner.install_dir.clone() else {
            return Err(format!(
                "module '{}' is unavailable: no directory is available to install modules into",
                record.id
            ));
        };
        let allow_download = self.inner.allow_download;
        let module_config = (self.inner.module_config)(record.id);
        let cache_root = root.clone();
        let bundled = self.inner.bundled_dir.clone();
        self.blocking(move || {
            Self::load_cached(
                runtime,
                record,
                &cache_root,
                &module_config,
                allow_download,
                bundled.as_deref(),
            )
        })
        .await?;
        prune_stale_versions(&root, record.id, record.version);
        Ok(())
    }

    /// Run a blocking module operation on the blocking pool. A panic in the
    /// loader is reported rather than propagated, and every error is marked
    /// terminal.
    async fn blocking<F>(&self, work: F) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        self.runtime()
            .await
            .map_err(|error| {
                self.mark_terminal(format!("the module bus could not start: {error}"))
            })?
            .blocking(work)
            .await
            .map_err(|error| self.mark_terminal(error))
    }

    /// Append the terminal-fault sentence to `error` unless it already has it.
    #[must_use]
    pub fn mark_terminal(&self, error: String) -> String {
        let marker = self.inner.terminal_marker.as_str();
        if marker.is_empty() || error.contains(marker) {
            return error;
        }
        format!("{error}. {marker}; restart the app to try again")
    }

    /// Load the pinned release for this host through the release cache.
    ///
    /// Tries the preferred artifact first and falls through on admission failure:
    /// a host newer than the newest published build runs that build, and one
    /// whose toolchain the newest artifact does not match falls back.
    ///
    /// # Errors
    ///
    /// The sanitised message from [`load_first_admitted`].
    pub fn load_cached(
        runtime: &ModuleRuntime,
        record: &ReleaseRecord,
        install_root: &Path,
        module_config: &serde_json::Value,
        allow_download: bool,
        bundled_root: Option<&Path>,
    ) -> Result<(), String> {
        let assets: Vec<ReleaseAsset<'static>> = host_candidates()
            .iter()
            .filter_map(|key| record.asset_for(key))
            .copied()
            .collect();
        let plan = ReleasePlan {
            id: record.id,
            version: record.version,
            release_url: record.release_url,
            assets: &assets,
            install_root,
            bundled_root,
            allow_download,
        };
        load_first_admitted(runtime.host(), &plan, module_config).map(|_| ())
    }

    /// Load a platform library from `path`.
    ///
    /// # Errors
    ///
    /// A terminal message; the library stays mapped whatever the outcome.
    pub fn load_local(
        runtime: &ModuleRuntime,
        path: &Path,
        id: &str,
        module_config: serde_json::Value,
    ) -> Result<(), String> {
        match runtime.host().load_file_with_config(path, module_config) {
            Ok(_) => {
                tracing::info!(module = id, "[tinybus::module] loaded from a local artifact");
                Ok(())
            }
            Err(err) => Err(format!(
                "module '{id}' could not be loaded from its local artifact: {err}. This is \
                 terminal for the running process; restart the app to try again"
            )),
        }
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
