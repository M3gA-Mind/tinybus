//! Immutable signal provenance, captured before delivery enters a subscriber queue.
//!
//! This is delivery identity, never tenant authority. A host must separately
//! admit publishers and bind their work to captured application authorization.

use crate::{BusName, message::Message};
use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

/// Opaque identity of one local connection, stable across its clones.
///
/// A host may retain and compare an explicitly admitted publisher identity.
/// Creating another connection, copying a header, or completing Hello cannot
/// reproduce it. This token has no wire representation or tenant meaning.
#[derive(Clone)]
pub struct LocalPublisherIdentity(Arc<()>);

impl LocalPublisherIdentity {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
}
impl PartialEq for LocalPublisherIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for LocalPublisherIdentity {}
impl Hash for LocalPublisherIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}
impl fmt::Debug for LocalPublisherIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalPublisherIdentity { .. }")
    }
}

/// Delivery path established by the receiving SDK, never a signal header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalOrigin {
    /// No verified broker or local publisher proof; refuse authority-dependent work.
    #[default]
    Unverified,
    /// A real broker delivery, with the same native proof used for method calls.
    Brokered,
    /// Publication through this connection's in-process local delivery entrypoint.
    Local,
}

/// Immutable out-of-band provenance for one delivered signal.
///
/// Sender names identify peers only within their broker session. Local identities
/// identify native publishers only while their captured connection binding is
/// admitted. Neither grants tenant access or authenticates an event body. Custom
/// transports and Unix sockets remain unverified, just as for CallContext.
///
/// Authority cannot be reconstructed from signal JSON:
/// ```compile_fail
/// let _: tinybus::SignalContext = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone, Default)]
pub struct SignalContext {
    origin: SignalOrigin,
    sender: Option<BusName>,
    local_publisher: Option<LocalPublisherIdentity>,
}
impl SignalContext {
    /// The SDK-captured delivery path. This is not permission to act.
    pub fn origin(&self) -> SignalOrigin {
        self.origin
    }
    /// A broker-stamped unique peer name only for proven broker delivery.
    ///
    /// Local publication does not authenticate Hello or a mutable sender header.
    pub fn authenticated_sender(&self) -> Option<&BusName> {
        self.sender.as_ref()
    }
    /// The exact local publisher identity, only for in-process local delivery.
    pub fn local_publisher(&self) -> Option<&LocalPublisherIdentity> {
        self.local_publisher.as_ref()
    }
}

/// A message and its immutable SDK-captured provenance in one queued delivery.
///
/// Inspecting/cloning a message cannot mint a verified delivery. Raw legacy
/// signal receivers remain available but expose no authority-bearing context.
#[derive(Debug, Clone)]
pub struct SignalDelivery {
    message: Message,
    context: SignalContext,
}
impl SignalDelivery {
    pub(crate) fn unverified(message: Message) -> Self {
        Self {
            message,
            context: SignalContext::default(),
        }
    }
    pub(crate) fn received(message: Message, brokered: bool) -> Self {
        if !brokered {
            return Self::unverified(message);
        }
        let sender = message
            .header
            .sender
            .as_ref()
            .filter(|name| name.is_unique())
            .cloned();
        Self {
            message,
            context: SignalContext {
                origin: SignalOrigin::Brokered,
                sender,
                local_publisher: None,
            },
        }
    }
    pub(crate) fn local(message: Message, publisher: LocalPublisherIdentity) -> Self {
        Self {
            message,
            context: SignalContext {
                origin: SignalOrigin::Local,
                sender: None,
                local_publisher: Some(publisher),
            },
        }
    }
    /// Protocol data, including untrusted body and header values.
    pub fn message(&self) -> &Message {
        &self.message
    }
    /// Immutable provenance captured before queueing.
    pub fn context(&self) -> &SignalContext {
        &self.context
    }
}

#[cfg(test)]
#[path = "signal_tests.rs"]
mod tests;
