//! Shared module access for hosts: a [`ModuleClient`] over a [`ModuleAccess`].
//!
//! Constructing a client starts nothing. The first call loads the module through
//! its [`ModuleAccess`] (a `Resolver`, with the `modules` feature), which
//! resolves once per process and caches the outcome.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};

use crate::module::failure::{FailureReporter, Reason};
use crate::module::release::ReleaseRecord;
use crate::{Error, Proxy};

/// A boxed future returned by a [`ModuleAccess`].
pub type AccessFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a [`ModuleAccess`] could not hand out a proxy for a serving module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyFailure {
    /// The module bus could not be started or reached.
    Transport,
    /// The module's bus name or object path is not well formed.
    Contract,
}

/// What a [`ModuleClient`] needs from the host to reach a registry module.
///
/// With the `modules` feature the host's `Resolver` implements it, loading the
/// module lazily. A host built without the loader implements it over its bare
/// registry and refuses every load, so callers compile and behave the same way
/// (every call is unavailable and reported) with or without the loader.
pub trait ModuleAccess: Send + Sync {
    /// The registry record for `id`, if the host has one.
    fn find(&self, id: &str) -> Option<&'static ReleaseRecord>;

    /// Whether modules are enabled at all.
    fn enabled(&self) -> bool;

    /// The reporter terminal outcomes go through.
    fn reporter(&self) -> &FailureReporter;

    /// Ensure `id` is loaded and serving.
    ///
    /// # Errors
    ///
    /// A message fit for a user or a model when the module cannot be loaded.
    fn ensure_loaded<'a>(&'a self, id: &'a str) -> AccessFuture<'a, Result<(), String>>;

    /// A proxy for `object_path` on the serving module at `bus_name`.
    ///
    /// # Errors
    ///
    /// [`ProxyFailure`] when the bus cannot be reached or the names are invalid.
    fn proxy<'a>(
        &'a self,
        bus_name: &'a str,
        object_path: &'a str,
    ) -> AccessFuture<'a, Result<Proxy, ProxyFailure>>;
}

/// A sanitised terminal module outcome. Arguments and remote error text are
/// absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleCallError {
    /// The module is not available.
    Unavailable,
    /// The module speaks a different wire contract.
    IncompatibleContract,
    /// The bus or its transport failed.
    TransportFailed,
    /// The module accepted the call and failed it.
    ModuleFault,
}

impl std::fmt::Display for ModuleCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Unavailable => "unavailable",
            Self::IncompatibleContract => "incompatible contract",
            Self::TransportFailed => "transport failed",
            Self::ModuleFault => "execution failed",
        };
        // Product reporting recognises this prefix as an already reported
        // module failure, rather than producing a second terminal event.
        write!(formatter, "MODULE_CALL_REPORTED: module {reason}")
    }
}

impl std::error::Error for ModuleCallError {}

/// Calls contract members on registry modules, loading them lazily.
#[derive(Clone)]
pub struct ModuleClient {
    access: Arc<dyn ModuleAccess>,
    timeout: Option<Duration>,
    /// Routes calls to a proxy instead of loading a module. For tests.
    fixture: Option<Proxy>,
}

impl ModuleClient {
    /// A client that reaches modules through `access` (a `Resolver`, with the
    /// `modules` feature).
    #[must_use]
    pub fn new(access: impl ModuleAccess + 'static) -> Self {
        Self {
            access: Arc::new(access),
            timeout: None,
            fixture: None,
        }
    }

    /// Route every call to `proxy` instead of loading a module. The module must
    /// still be in the registry and modules must still be enabled.
    #[must_use]
    pub fn with_fixture(mut self, proxy: Proxy) -> Self {
        self.fixture = Some(proxy);
        self
    }

    /// The fixture proxy, if one is set.
    #[must_use]
    pub fn fixture_proxy(&self) -> Option<Proxy> {
        self.fixture.clone()
    }

    /// Override the TinyBus deadline for every call made through this client,
    /// confidential calls and fixtures included.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Execute a contract member.
    ///
    /// No implementation fallback is provided. The caller owns approvals,
    /// cancellation and lifecycle cleanup.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`]; the cause is reported once through the
    /// resolver's reporter.
    pub async fn call<R: DeserializeOwned>(
        &self,
        module: &str,
        member: &str,
        args: impl Serialize,
    ) -> Result<R, ModuleCallError> {
        self.invoke(module, member, args, false).await
    }

    /// Execute a confidential member after TinyBus recipient attestation.
    ///
    /// A failure to attest the pinned artifact fails the call; ordinary delivery
    /// is never retried.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`].
    pub async fn call_confidential<R: DeserializeOwned>(
        &self,
        module: &str,
        member: &str,
        args: impl Serialize,
    ) -> Result<R, ModuleCallError> {
        self.invoke(module, member, args, true).await
    }

    /// Call a member that returns a JSON-encoded string and deserialise that
    /// inner document. Malformed output is reported with only the registered
    /// module identity and the closed `module_fault` reason.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`].
    pub async fn call_json<R: DeserializeOwned>(
        &self,
        module: &str,
        member: &str,
        args: impl Serialize,
    ) -> Result<R, ModuleCallError> {
        let response: String = self.call(module, member, args).await?;
        self.decode_json(module, &response)
    }

    /// Confidential counterpart to [`Self::call_json`]. A decode failure is
    /// terminal; the call is never retried through ordinary delivery.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`].
    pub async fn call_confidential_json<R: DeserializeOwned>(
        &self,
        module: &str,
        member: &str,
        args: impl Serialize,
    ) -> Result<R, ModuleCallError> {
        let response: String = self.call_confidential(module, member, args).await?;
        self.decode_json(module, &response)
    }

    /// A validated proxy for an object path returned by an owning module.
    ///
    /// Object paths belong inside the host adapter; wrap this in a
    /// domain-specific client rather than exposing the path as product state.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`].
    pub async fn object_client(
        &self,
        module: &str,
        object_path: &str,
    ) -> Result<ModuleObjectClient, ModuleCallError> {
        let record = self.record(module)?;
        if let Some(fixture) = &self.fixture {
            let proxy = fixture
                .connection()
                .proxy(
                    fixture.destination().as_str(),
                    object_path,
                    fixture.interface().as_str(),
                )
                .map_err(|_| ModuleCallError::IncompatibleContract)?;
            return Ok(self.object(record, proxy));
        }
        let proxy = self.load_proxy(record, object_path).await?;
        Ok(self.object(record, proxy))
    }

    /// Report a structurally invalid reply from a known module invocation.
    pub fn report_malformed_reply(&self, module: &str) {
        if let Some(record) = self.access.find(module) {
            report_invocation(&*self.access, record, Reason::ModuleFault);
        }
    }

    /// Report a registered module that advertises an older wire contract.
    pub fn report_incompatible_contract(&self, module: &str) {
        if let Some(record) = self.access.find(module) {
            report(&*self.access, record, Reason::IncompatibleContract);
        }
    }

    /// Report a known module that does not provide the requested capability.
    pub fn report_unavailable(&self, module: &str) {
        if let Some(record) = self.access.find(module) {
            report(&*self.access, record, Reason::ModuleUnavailable);
        }
    }

    fn object(&self, record: &'static ReleaseRecord, proxy: Proxy) -> ModuleObjectClient {
        ModuleObjectClient {
            access: Arc::clone(&self.access),
            record,
            proxy: self.with_deadline(proxy),
        }
    }

    fn with_deadline(&self, proxy: Proxy) -> Proxy {
        self.timeout
            .map_or(proxy.clone(), |timeout| proxy.with_timeout(timeout))
    }

    fn record(&self, module: &str) -> Result<&'static ReleaseRecord, ModuleCallError> {
        let Some(record) = self.access.find(module) else {
            self.access.reporter().report_unknown_module();
            return Err(ModuleCallError::Unavailable);
        };
        if !self.access.enabled() {
            report(&*self.access, record, Reason::Disabled);
            return Err(ModuleCallError::Unavailable);
        }
        Ok(record)
    }

    /// Load `record` and build a proxy for `object_path` on it.
    async fn load_proxy(
        &self,
        record: &'static ReleaseRecord,
        object_path: &str,
    ) -> Result<Proxy, ModuleCallError> {
        self.access
            .ensure_loaded(record.id)
            .await
            .map_err(|_| ModuleCallError::Unavailable)?;
        self.access
            .proxy(record.bus_name, object_path)
            .await
            .map_err(|failure| match failure {
                ProxyFailure::Transport => {
                    report(&*self.access, record, Reason::TransportFailed);
                    ModuleCallError::TransportFailed
                }
                ProxyFailure::Contract => {
                    report(&*self.access, record, Reason::IncompatibleContract);
                    ModuleCallError::IncompatibleContract
                }
            })
    }

    async fn invoke<R: DeserializeOwned>(
        &self,
        module: &str,
        member: &str,
        args: impl Serialize,
        confidential: bool,
    ) -> Result<R, ModuleCallError> {
        let record = self.record(module)?;
        let proxy = if let Some(fixture) = &self.fixture {
            fixture.clone()
        } else {
            self.load_proxy(record, record.object_path).await?
        };
        let proxy = self.with_deadline(proxy);
        invoke_proxy(&*self.access, record, &proxy, member, args, confidential).await
    }

    fn decode_json<R: DeserializeOwned>(
        &self,
        module: &str,
        response: &str,
    ) -> Result<R, ModuleCallError> {
        serde_json::from_str(response).map_err(|_| {
            self.report_malformed_reply(module);
            ModuleCallError::ModuleFault
        })
    }
}

/// A validated proxy for a module-owned object path.
///
/// Bus object identities do not become product state or leak into logs and
/// controller responses.
#[derive(Clone)]
pub struct ModuleObjectClient {
    access: Arc<dyn ModuleAccess>,
    record: &'static ReleaseRecord,
    proxy: Proxy,
}

impl ModuleObjectClient {
    /// Call a member of the object.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`].
    pub async fn call<R: DeserializeOwned>(
        &self,
        member: &str,
        args: impl Serialize,
    ) -> Result<R, ModuleCallError> {
        invoke_proxy(&*self.access, self.record, &self.proxy, member, args, false).await
    }

    /// Call a confidential member of the object.
    ///
    /// # Errors
    ///
    /// A sanitised [`ModuleCallError`].
    pub async fn call_confidential<R: DeserializeOwned>(
        &self,
        member: &str,
        args: impl Serialize,
    ) -> Result<R, ModuleCallError> {
        invoke_proxy(&*self.access, self.record, &self.proxy, member, args, true).await
    }
}

async fn invoke_proxy<R: DeserializeOwned>(
    access: &dyn ModuleAccess,
    record: &'static ReleaseRecord,
    proxy: &Proxy,
    member: &str,
    args: impl Serialize,
    confidential: bool,
) -> Result<R, ModuleCallError> {
    let result = if confidential {
        proxy.call_confidential(member, args).await
    } else {
        proxy.call(member, args).await
    };
    result.map_err(|error| report_proxy_failure(access.reporter(), record, &error))
}

/// Report a bus failure without exposing its payload. Product adapters may
/// separately decode a structured provider refusal for display.
pub fn report_proxy_failure(
    reporter: &FailureReporter,
    record: &ReleaseRecord,
    error: &Error,
) -> ModuleCallError {
    let (outcome, reason) = classify(error);
    if reason == Reason::ModuleUnavailable {
        reporter.report(record.id, record.version, reason);
    } else {
        reporter.report_invocation(record.id, record.version, reason);
    }
    outcome
}

fn report(access: &dyn ModuleAccess, record: &ReleaseRecord, reason: Reason) {
    access.reporter().report(record.id, record.version, reason);
}

fn report_invocation(access: &dyn ModuleAccess, record: &ReleaseRecord, reason: Reason) {
    access
        .reporter()
        .report_invocation(record.id, record.version, reason);
}

/// Map a bus error to a sanitised outcome and its closed reason.
#[must_use]
pub fn classify(error: &Error) -> (ModuleCallError, Reason) {
    match error {
        Error::ModuleUnavailable { .. } => {
            (ModuleCallError::Unavailable, Reason::ModuleUnavailable)
        }
        Error::IncompatibleVersion { .. }
        | Error::UnknownMethod { .. }
        | Error::UnknownInterface { .. }
        | Error::UnknownObject { .. }
        | Error::BadArguments { .. }
        | Error::Json(_) => (
            ModuleCallError::IncompatibleContract,
            Reason::IncompatibleContract,
        ),
        Error::MethodFailed { name, .. }
            if name == "ai.tinyhumans.tinybus.Error.ModuleUnavailable" =>
        {
            (ModuleCallError::Unavailable, Reason::ModuleUnavailable)
        }
        Error::MethodFailed { name, .. }
            if matches!(
                name.as_str(),
                Error::UNKNOWN_METHOD
                    | "ai.tinyhumans.tinybus.Error.IncompatibleVersion"
                    | "ai.tinyhumans.tinybus.Error.UnknownInterface"
                    | "ai.tinyhumans.tinybus.Error.UnknownObject"
                    | "ai.tinyhumans.tinybus.Error.BadArguments"
            ) =>
        {
            (
                ModuleCallError::IncompatibleContract,
                Reason::IncompatibleContract,
            )
        }
        Error::MethodFailed { name, .. }
            if matches!(
                name.as_str(),
                Error::NOT_ATTESTED | Error::CONFIDENTIALITY_REQUIRED
            ) =>
        {
            (ModuleCallError::TransportFailed, Reason::TransportFailed)
        }
        Error::MethodFailed { .. } => (ModuleCallError::ModuleFault, Reason::ModuleFault),
        _ => (ModuleCallError::TransportFailed, Reason::TransportFailed),
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
