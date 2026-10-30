//! Sidecar lifecycle status, log/plugin shapes, and the host-service trait.

use std::fmt;

use async_trait::async_trait;
use serde_json::Value;

use crate::domain::Domain;
use crate::hook::Hook;
use crate::info::{PluginFeatures, PluginSource, PluginState};
use crate::rpc::RpcError;

/// Sidecar lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarStatus {
    /// Spawned; `load` handshake in flight.
    Starting,
    /// Handshake completed; requests are served.
    Ready,
    /// Unusable, with the reason (crash, malformed protocol, spawn failure).
    Failed(String),
    /// Shut down on purpose.
    Stopped,
}

impl SidecarStatus {
    /// Whether requests can be served.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Failure reason, if failed.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

impl fmt::Display for SidecarStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => formatter.write_str("starting"),
            Self::Ready => formatter.write_str("ready"),
            Self::Failed(reason) => write!(formatter, "failed: {reason}"),
            Self::Stopped => formatter.write_str("stopped"),
        }
    }
}

/// One `log` notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarLog {
    /// `debug`, `info`, `warn` or `error`.
    pub level: String,
    /// Message text.
    pub message: String,
    /// Owning plugin, when the sidecar reported one.
    pub plugin: Option<String>,
}

/// A plugin announced by the sidecar `register` notification.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarPlugin {
    /// Plugin id.
    pub id: String,
    /// Declared entrypoints.
    pub features: PluginFeatures,
    /// Supported hooks.
    pub hooks: Vec<Hook>,
    /// Supported transform domains.
    pub transforms: Vec<Domain>,
    /// Current state (`active`, or `failed` with the reason).
    pub state: PluginState,
    /// Source reported by the sidecar.
    pub source: PluginSource,
}

/// Host services a plugin may call while a hook runs.
///
/// The sidecar correlates calls with `requestId`; the bridge enforces the
/// request deadline and returns a JSON-RPC error on timeout or absence.
#[async_trait]
pub trait HostServices: Send + Sync {
    /// Handle one callback (session read, permission, storage, event subscribe).
    async fn callback(
        &self,
        request_id: &str,
        method: &str,
        args: &Value,
    ) -> Result<Value, RpcError>;
}
