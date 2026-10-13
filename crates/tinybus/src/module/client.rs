//! Shared module access for hosts: a [`ModuleClient`] over a [`Resolver`].
//!
//! Constructing a client starts nothing. The first call loads the module through
//! the resolver, which resolves once per process and caches the outcome.

use std::time::Duration;

use serde::{Serialize, de::DeserializeOwned};

use crate::module::failure::Reason;
use crate::module::resolve::{ReleaseRecord, Resolver};
use crate::{Error, Proxy};

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
    resolver: Resolver,
    timeout: Option<Duration>,
    /// Routes calls to a proxy instead of loading a module. For tests.
    fixture: Option<Proxy>,
}

impl ModuleClient {
    /// A client that loads modules through `resolver`.
    #[must_use]
    pub fn new(resolver: Resolver) -> Self {
        Self {
            resolver,
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
        if let Some(record) = self.resolver.find(module) {
            self.resolver.report_invocation(record, Reason::ModuleFault);
        }
    }

    /// Report a registered module that advertises an older wire contract.
    pub fn report_incompatible_contract(&self, module: &str) {
        if let Some(record) = self.resolver.find(module) {
            self.resolver.report(record, Reason::IncompatibleContract);
        }
    }

    /// Report a known module that does not provide the requested capability.
    pub fn report_unavailable(&self, module: &str) {
        if let Some(record) = self.resolver.find(module) {
            self.resolver.report(record, Reason::ModuleUnavailable);
        }
    }

    fn object(&self, record: &'static ReleaseRecord, proxy: Proxy) -> ModuleObjectClient {
        ModuleObjectClient {
            resolver: self.resolver.clone(),
            record,
            proxy: self.with_deadline(proxy),
        }
    }

    fn with_deadline(&self, proxy: Proxy) -> Proxy {
        self.timeout
            .map_or(proxy.clone(), |timeout| proxy.with_timeout(timeout))
    }

    fn record(&self, module: &str) -> Result<&'static ReleaseRecord, ModuleCallError> {
        let Some(record) = self.resolver.find(module) else {
            self.resolver.reporter().report_unknown_module();
            return Err(ModuleCallError::Unavailable);
        };
        if !self.resolver.enabled() {
            self.resolver.report(record, Reason::Disabled);
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
        self.resolver
            .ensure_loaded(record.id)
            .await
            .map_err(|_| ModuleCallError::Unavailable)?;
        let runtime = self.resolver.runtime().await.map_err(|_| {
            self.resolver.report(record, Reason::TransportFailed);
            ModuleCallError::TransportFailed
        })?;
        runtime.proxy(record.bus_name, object_path).map_err(|_| {
            self.resolver.report(record, Reason::IncompatibleContract);
            ModuleCallError::IncompatibleContract
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
        invoke_proxy(&self.resolver, record, &proxy, member, args, confidential).await
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
    resolver: Resolver,
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
        invoke_proxy(&self.resolver, self.record, &self.proxy, member, args, false).await
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
        invoke_proxy(&self.resolver, self.record, &self.proxy, member, args, true).await
    }
}

async fn invoke_proxy<R: DeserializeOwned>(
    resolver: &Resolver,
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
    result.map_err(|error| report_proxy_failure(resolver, record, &error))
}

/// Report a bus failure without exposing its payload. Product adapters may
/// separately decode a structured provider refusal for display.
pub fn report_proxy_failure(
    resolver: &Resolver,
    record: &ReleaseRecord,
    error: &Error,
) -> ModuleCallError {
    let (outcome, reason) = classify(error);
    if reason == Reason::ModuleUnavailable {
        resolver.report(record, reason);
    } else {
        resolver.report_invocation(record, reason);
    }
    outcome
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
