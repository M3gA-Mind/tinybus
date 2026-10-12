//! Local, exact-instance confidential callbacks; never artifact attestation.

use super::*;
use crate::message::Header;
use crate::transport::memory::MemoryTransport;
use crate::{CallContext, Connection, Interface, InterfaceName, MemberName, ObjectPath};
use async_trait::async_trait;
use tokio::sync::Notify;

/// An exact callback interface and its finite member allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCallbackEndpoint {
    /// Fixed object path; no subtree matching.
    pub path: ObjectPath,
    /// Fixed interface; no alias matching.
    pub interface: InterfaceName,
    /// Permitted methods, between one and 64 distinct names.
    pub members: Vec<MemberName>,
}

struct Lifetime {
    active: bool,
    used: bool,
    revoked: bool,
    module: Option<BusName>,
    running: usize,
    served: Vec<(ObjectPath, InterfaceName)>,
    registering: Vec<(ObjectPath, InterfaceName)>,
}

pub(crate) struct CallbackState {
    pub(crate) destination: BusName,
    endpoints: Vec<HostCallbackEndpoint>,
    module_transport: Mutex<Option<std::sync::Weak<ModuleTransport>>>,
    lifetime: Mutex<Lifetime>,
    drained: Notify,
}

impl CallbackState {
    fn module_live(&self) -> bool {
        self.module_transport
            .lock()
            .expect("callback module lock")
            .as_ref()
            .is_none_or(|module| module.upgrade().is_some_and(|module| !module.is_faulted()))
    }

    pub(crate) fn module_peer(&self) -> Option<BusName> {
        self.lifetime
            .lock()
            .expect("callback lifetime lock")
            .module
            .clone()
    }

    pub(crate) fn includes_peer(&self, peer: &BusName) -> bool {
        &self.destination == peer || self.module_peer().as_ref() == Some(peer)
    }

    pub(crate) fn revoke(&self) {
        let mut state = self.lifetime.lock().expect("callback lifetime lock");
        state.active = false;
        state.revoked = true;
    }

    pub(crate) fn matches(&self, header: &Header) -> bool {
        let state = self.lifetime.lock().expect("callback lifetime lock");
        state.active
            && self.module_live()
            && header.confidential
            && header.sender.as_ref() == state.module.as_ref()
            && state.module.is_some()
            && header.destination.as_ref() == Some(&self.destination)
            && self.endpoints.iter().any(|endpoint| {
                header.path.as_ref() == Some(&endpoint.path)
                    && header.interface.as_ref() == Some(&endpoint.interface)
                    && header
                        .member
                        .as_ref()
                        .is_some_and(|member| endpoint.members.contains(member))
            })
    }
}

/// Read-only liveness and authenticated module identity for a callback worker.
///
/// Check again after awaits and before publishing provider results. Revocation
/// prevents new work but cannot undo a provider write already in progress.
#[derive(Clone)]
pub struct HostCallbackGuard(Arc<CallbackState>);

impl HostCallbackGuard {
    /// The exact loaded module peer, installed before initialization can call back.
    pub fn module_peer(&self) -> Option<BusName> {
        self.0.module_peer()
    }

    /// Whether this binding remains active for this authenticated sender.
    pub fn authorizes(&self, context: &CallContext) -> bool {
        let state = self.0.lifetime.lock().expect("callback lifetime lock");
        state.active
            && self.0.module_live()
            && context.is_confidential()
            && state.module.is_some()
            && context.authenticated_sender() == state.module.as_ref()
    }

    /// Number of callbacks whose user futures have not yet completed or dropped.
    /// A caller timeout does not remove an executing callback from this count.
    pub fn running(&self) -> usize {
        self.0
            .lifetime
            .lock()
            .expect("callback lifetime lock")
            .running
    }

    /// Wait for currently running user futures to complete or drop.
    /// Revoke first to prevent new admission. This proves no provider activity
    /// beyond the lifetime of the user future; detached provider tasks need
    /// their own completion fence.
    pub async fn drained(&self) {
        loop {
            let changed = self.0.drained.notified();
            if self.running() == 0 {
                return;
            }
            changed.await;
        }
    }
}

/// SDK-created local callback recipient, scoped once to one verified module.
///
/// This is intentionally not an attestation and cannot enroll an existing
/// connection, custom transport, well-known alias, or remote recipient. Drop
/// revokes immediately; explicit close also closes the owned local connection.
pub struct HostCallbackBinding {
    owner: std::sync::Weak<ModuleHostInner>,
    broker: Broker,
    connection: Connection,
    state: Arc<CallbackState>,
    transport: Arc<MemoryTransport>,
}

/// Outgoing operations on the binding's private connection.
///
/// This handle deliberately has no connection accessor, dereference,
/// conversion, or service registration surface. Clones retain only this API.
#[derive(Clone, Debug)]
pub struct HostCallbackProxy(crate::Proxy);

impl HostCallbackProxy {
    /// Override the deadline for outgoing calls.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.0 = self.0.with_timeout(timeout);
        self
    }
    /// The destination addressed by this handle.
    pub fn destination(&self) -> &BusName {
        self.0.destination()
    }
    /// The addressed object path.
    pub fn path(&self) -> &ObjectPath {
        self.0.path()
    }
    /// The addressed interface.
    pub fn interface(&self) -> &InterfaceName {
        self.0.interface()
    }
    /// Call a method and deserialize its ordinary reply.
    pub async fn call<R: serde::de::DeserializeOwned>(
        &self,
        member: &str,
        args: impl serde::Serialize,
    ) -> Result<R> {
        self.0.call(member, args).await
    }
    /// Call a method receiving a bounded streaming reply.
    pub async fn call_streaming<R: serde::de::DeserializeOwned>(
        &self,
        member: &str,
        args: impl serde::Serialize,
    ) -> Result<R> {
        self.0.call_streaming(member, args).await
    }
    /// Call an attested recipient with a confidential body.
    pub async fn call_confidential<R: serde::de::DeserializeOwned>(
        &self,
        member: &str,
        args: impl serde::Serialize,
    ) -> Result<R> {
        self.0.call_confidential(member, args).await
    }
    /// Call a broker module-control method with a redacted body.
    pub async fn call_sensitive<R: serde::de::DeserializeOwned>(
        &self,
        member: &str,
        args: impl serde::Serialize,
    ) -> Result<R> {
        self.0.call_sensitive(member, args).await
    }
    /// Read the broker's artifact attestation for the destination.
    pub async fn attestation(&self) -> Result<Option<crate::attest::Attestation>> {
        self.0.attestation().await
    }
    /// Whether the destination currently has an owner.
    pub async fn is_available(&self) -> Result<bool> {
        self.0.is_available().await
    }
    /// Subscribe to signals for the addressed object and interface.
    pub async fn receive_signal(
        &self,
        member: &str,
    ) -> Result<tokio::sync::broadcast::Receiver<crate::message::Message>> {
        self.0.receive_signal(member).await
    }
}

impl HostCallbackBinding {
    /// The exact SDK-created host destination for module configuration.
    pub fn unique_name(&self) -> &BusName {
        &self.state.destination
    }
    /// Construct an outgoing proxy using this private host connection.
    ///
    /// The outgoing handle cannot expose the owned callback connection:
    /// ```compile_fail
    /// # async fn example(binding: &tinybus::module::HostCallbackBinding) -> tinybus::Result<()> {
    /// let outgoing = binding.proxy("org.example.Module", "/module", "org.example.Module")?;
    /// let _: &tinybus::Connection = outgoing.connection();
    /// # Ok(())
    /// # }
    /// ```
    /// The proxy exposes no raw connection or service registration escape.
    pub fn proxy(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
    ) -> Result<HostCallbackProxy> {
        self.connection
            .proxy(destination, path, interface)
            .map(HostCallbackProxy)
    }
    /// Read-only capability state for domain authorization and completion accounting.
    pub fn guard(&self) -> HostCallbackGuard {
        HostCallbackGuard(self.state.clone())
    }
    /// Immediately and idempotently prevent new route and user-dispatch admission.
    /// Running user futures remain accounted for until actual completion/drop.
    pub fn revoke(&self) {
        self.broker
            .router
            .lock()
            .expect("router lock")
            .revoke_host_callbacks(&self.state);
    }
    /// Revoke and close the local connection. This does not cancel running user code.
    pub async fn close(&self) -> Result<()> {
        self.revoke();
        self.connection.close().await
    }
    /// Serve a declared endpoint with an unbypassable authenticated live guard.
    /// Every declared member must be implemented; duplicate registration fails.
    pub async fn serve_at(&self, path: ObjectPath, interface: impl Interface) -> Result<()> {
        let name = interface.name();
        let endpoint = self
            .state
            .endpoints
            .iter()
            .find(|endpoint| endpoint.path == path && endpoint.interface == name)
            .ok_or_else(|| Error::failed("callback endpoint is not declared"))?;
        let provided = interface.members();
        if !endpoint
            .members
            .iter()
            .all(|member| provided.contains(member))
        {
            return Err(Error::failed("callback endpoint members are missing"));
        }
        {
            let mut state = self.state.lifetime.lock().expect("callback lifetime lock");
            if state.used
                || state.revoked
                || state.served.contains(&(path.clone(), name.clone()))
                || state.registering.contains(&(path.clone(), name.clone()))
            {
                return Err(Error::failed(
                    "callback registration is immutable or duplicated",
                ));
            }
            state.registering.push((path.clone(), name.clone()));
        }
        let result = self
            .connection
            .serve_at(
                path.clone(),
                GuardedInterface {
                    interface,
                    endpoint: endpoint.clone(),
                    state: self.state.clone(),
                },
            )
            .await;
        {
            let mut state = self.state.lifetime.lock().expect("callback lifetime lock");
            state
                .registering
                .retain(|entry| entry != &(path.clone(), name.clone()));
            if result.is_ok() {
                state.served.push((path, name));
            }
        }
        result
    }
}

impl Drop for HostCallbackBinding {
    fn drop(&mut self) {
        self.revoke();
        // Drop cannot await. Closing the sender synchronously detaches the
        // owned peer without spawning a task that might never be polled.
        self.transport.close_outbound();
        self.broker
            .router
            .lock()
            .expect("router lock")
            .detach_unique(&self.state.destination);
    }
}

struct GuardedInterface<I> {
    interface: I,
    endpoint: HostCallbackEndpoint,
    state: Arc<CallbackState>,
}

struct Running(Arc<CallbackState>);
impl Drop for Running {
    fn drop(&mut self) {
        let mut state = self.0.lifetime.lock().expect("callback lifetime lock");
        state.running -= 1;
        if state.running == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

#[async_trait]
impl<I: Interface> Interface for GuardedInterface<I> {
    fn name(&self) -> InterfaceName {
        self.endpoint.interface.clone()
    }
    fn members(&self) -> Vec<MemberName> {
        self.endpoint.members.clone()
    }
    async fn call(&self, _: &MemberName, _: serde_json::Value) -> Result<serde_json::Value> {
        Err(Error::failed("authenticated scoped callback required"))
    }
    async fn call_with_context(
        &self,
        member: &MemberName,
        args: serde_json::Value,
        context: &CallContext,
    ) -> Result<serde_json::Value> {
        // Serialized with revoke: queued deliveries cannot bypass the live
        // check by carrying an earlier route admission through an await.
        let _running = {
            let mut state = self.state.lifetime.lock().expect("callback lifetime lock");
            if !self.state.module_live()
                || !context.is_confidential()
                || !state.active
                || state.module.is_none()
                || context.authenticated_sender() != state.module.as_ref()
                || !self.endpoint.members.contains(member)
            {
                return Err(Error::failed("scoped callback authority is unavailable"));
            }
            state.running += 1;
            Running(self.state.clone())
        };
        self.interface
            .call_with_context(member, args, context)
            .await
    }
}

impl ModuleHost {
    /// Create a private local recipient for at most 32 exact callback interfaces.
    /// No authority exists until a verified module is loaded with this binding.
    pub async fn prepare_host_callbacks(
        &self,
        endpoints: Vec<HostCallbackEndpoint>,
    ) -> Result<HostCallbackBinding> {
        if endpoints.is_empty()
            || endpoints.len() > 32
            || endpoints.iter().any(|endpoint| {
                endpoint.members.is_empty()
                    || endpoint.members.len() > 64
                    || endpoint
                        .members
                        .iter()
                        .enumerate()
                        .any(|(i, member)| endpoint.members[..i].contains(member))
            })
            || endpoints.iter().enumerate().any(|(i, endpoint)| {
                endpoints[..i].iter().any(|previous| {
                    previous.path == endpoint.path && previous.interface == endpoint.interface
                })
            })
        {
            return Err(Error::failed("callback endpoint declaration is invalid"));
        }
        let (host, peer) = MemoryTransport::pair();
        let unique = self.inner.broker.attach(Arc::new(peer));
        let transport = Arc::new(host);
        let mut rollback = PeerRollback {
            broker: self.inner.broker.clone(),
            peer: unique,
            transport: transport.clone(),
            committed: false,
        };
        let connection = Connection::attach(transport.clone());
        connection.handshake().await?;
        let destination = connection
            .unique_name()
            .ok_or_else(|| Error::failed("callback connection has no unique name"))?;
        rollback.committed = true;
        Ok(HostCallbackBinding {
            owner: Arc::downgrade(&self.inner),
            broker: self.inner.broker.clone(),
            connection,
            transport,
            state: Arc::new(CallbackState {
                destination,
                endpoints,
                module_transport: Mutex::new(None),
                lifetime: Mutex::new(Lifetime {
                    active: false,
                    used: false,
                    revoked: false,
                    module: None,
                    running: 0,
                    served: Vec::new(),
                    registering: Vec::new(),
                }),
                drained: Notify::new(),
            }),
        })
    }

    /// Load a hash-verified file and bind its exact peer before any initialization.
    ///
    /// All endpoints must already be served. The binding is single-use even on
    /// failure. Configuration is frozen for reinitialization. Generic load APIs
    /// grant no callback authority. The caller supplies the binding's unique
    /// destination in its domain-specific configuration; it cannot change scope.
    pub async fn load_file_with_host_callbacks(
        &self,
        path: impl AsRef<Path>,
        config: serde_json::Value,
        binding: &HostCallbackBinding,
    ) -> Result<ModuleInfo> {
        if !Arc::ptr_eq(&self.inner.broker.router, &binding.broker.router)
            || !binding
                .owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.inner))
        {
            return Err(Error::failed(
                "callback binding belongs to another host owner",
            ));
        }
        {
            let mut state = binding
                .state
                .lifetime
                .lock()
                .expect("callback lifetime lock");
            if state.used || state.revoked || state.served.len() != binding.state.endpoints.len() {
                return Err(Error::failed(
                    "callback binding is already used or endpoints are missing",
                ));
            }
            state.used = true;
        }
        let mut rollback = LoadRollback {
            binding,
            host: self,
            transport: None,
            committed: false,
        };
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            check_directory(parent)?;
        }
        check_file(path)?;
        let hash = allowlisted_hash(path, std::fs::File::open(path)?)?.ok_or_else(|| {
            Error::module_refused(path, "scoped callbacks require a verified artifact")
        })?;
        let artifact = loader::load(path, self.inner.strict.load(Ordering::Acquire))?;
        self.ensure_dependencies(&artifact.manifest, path)?;
        if allowlisted_hash(path, std::fs::File::open(path)?)?.as_ref() != Some(&hash) {
            return Err(Error::module_refused(
                path,
                "verified artifact changed during loading",
            ));
        }
        let transport = {
            let _admission = self.inner.admission.lock().expect("module admission lock");
            let admitted = self.validate(path, &artifact.descriptor, &artifact.manifest)?;
            if self
                .inner
                .loaded
                .lock()
                .expect("module list lock")
                .iter()
                .any(|module| module.info.name == admitted.name)
            {
                return Err(Error::module_refused(path, "module name is already loaded"));
            }
            // Reservation happens inside attach_transport. Initialization is
            // deferred even for an eager module until routes and identity exist.
            let bytes = serde_json::to_vec(&config)
                .map_err(|_| Error::failed("module configuration is invalid"))?;
            let fingerprint = callback_config_fingerprint(&bytes);
            drop(config);
            let (transport, host) = ModuleTransport::new(admitted.name.clone(), bytes);
            transport.pause_initialization();
            transport.defer_initialize(artifact.init, host);
            let name = admitted.name.clone();
            rollback.transport = Some(transport.clone());
            self.attach_transport(
                path,
                admitted,
                artifact.manifest,
                transport.clone(),
                Activation {
                    lazy_init: true,
                    lazy_load: false,
                    descriptor_info: None,
                    pinned_sha256: Some(hash),
                },
            )?;
            rollback.transport = Some(transport.clone());
            let mut loaded = self.inner.loaded.lock().expect("module list lock");
            let module = loaded
                .iter_mut()
                .find(|module| module.info.name == name)
                .expect("module was attached");
            {
                let mut state = binding
                    .state
                    .lifetime
                    .lock()
                    .expect("callback lifetime lock");
                if state.revoked || state.module.is_some() {
                    return Err(Error::failed("callback binding was concurrently used"));
                }
                *binding
                    .state
                    .module_transport
                    .lock()
                    .expect("callback module lock") = Some(Arc::downgrade(&transport));
                state.module = Some(module.unique_name.clone());
                state.active = true;
            }
            module.callback_config = Some(fingerprint);
            self.inner
                .broker
                .router
                .lock()
                .expect("router lock")
                .add_host_callbacks(binding.state.clone())?;
            transport.release_initialization(true);
            transport
        };
        let initialized = transport.ensure_initialized().await;
        let ready = if initialized.is_ok() {
            tokio::time::timeout(Duration::from_secs(5), transport.wait_ready())
                .await
                .is_ok()
                && transport.is_ready()
        } else {
            false
        };
        if !ready {
            binding.revoke();
            let _ = transport.stop_sync(Duration::ZERO);
            let _ = transport.close().await;
            self.inner
                .loaded
                .lock()
                .expect("module list lock")
                .retain(|module| !Arc::ptr_eq(&module.transport, &transport));
            return Err(Error::module_refused(path, "module initialization failed"));
        }
        let loaded = self.inner.loaded.lock().expect("module list lock");
        let info = loaded
            .iter()
            .find(|module| Arc::ptr_eq(&module.transport, &transport))
            .ok_or_else(|| {
                Error::module_refused(path, "module was removed before load completion")
            })?
            .snapshot();
        let lifetime = binding
            .state
            .lifetime
            .lock()
            .expect("callback lifetime lock");
        // Snapshot first, then validate liveness at the commit point. A fault
        // racing this point may be a subsequent lifecycle event, but must never
        // turn the returned successful snapshot into Faulted or Stopped.
        if lifetime.revoked
            || !lifetime.active
            || !transport.is_ready()
            || !matches!(info.state, ModuleState::Ready)
        {
            return Err(Error::module_refused(
                path,
                "module became unavailable before load completion",
            ));
        }
        rollback.committed = true;
        Ok(info)
    }
}

struct LoadRollback<'a> {
    binding: &'a HostCallbackBinding,
    host: &'a ModuleHost,
    transport: Option<Arc<ModuleTransport>>,
    committed: bool,
}
impl Drop for LoadRollback<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.binding.revoke();
        self.binding.transport.close_outbound();
        self.binding
            .broker
            .router
            .lock()
            .expect("router lock")
            .detach_unique(&self.binding.state.destination);
        if let Some(transport) = &self.transport {
            transport.release_initialization(false);
            let peer = self.binding.state.module_peer().or_else(|| {
                self.host
                    .inner
                    .loaded
                    .lock()
                    .expect("module list lock")
                    .iter()
                    .find(|module| Arc::ptr_eq(&module.transport, transport))
                    .map(|module| module.unique_name.clone())
            });
            if let Some(peer) = peer {
                self.host
                    .inner
                    .broker
                    .router
                    .lock()
                    .expect("router lock")
                    .detach_unique(&peer);
            }
            // A timed-out initializer may still run. Stop is best effort;
            // revocation/detach removes authority without claiming cessation.
            let _ = transport.stop_sync(Duration::ZERO);
            self.host
                .inner
                .loaded
                .lock()
                .expect("module list lock")
                .retain(|module| !Arc::ptr_eq(&module.transport, transport));
        }
    }
}

impl Drop for ModuleHostInner {
    fn drop(&mut self) {
        for module in self.loaded.get_mut().expect("module list lock").iter() {
            self.broker
                .router
                .lock()
                .expect("router lock")
                .revoke_callbacks_for_peer(&module.unique_name);
        }
    }
}

struct PeerRollback {
    broker: Broker,
    peer: BusName,
    transport: Arc<MemoryTransport>,
    committed: bool,
}
impl Drop for PeerRollback {
    fn drop(&mut self) {
        if !self.committed {
            self.transport.close_outbound();
            self.broker
                .router
                .lock()
                .expect("router lock")
                .detach_unique(&self.peer);
        }
    }
}

#[cfg(test)]
#[path = "host_callbacks_tests.rs"]
mod tests;
