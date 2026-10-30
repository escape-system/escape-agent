//! MCP client public types, connection options and handler aliases.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Protocol, TimeoutConfig};
use crate::error::McpResult;

/// Maximum catalog pages followed before declaring a cursor loop.
pub(super) const MAX_PAGES: usize = 1_000;

/// Connection status, tagged by `status` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpStatus {
    /// Handshake and initial catalog completed.
    Connected,
    /// Connection is in progress.
    Pending,
    /// Configured but disabled.
    Disabled,
    /// Connection failed.
    Failed { error: String },
    /// The server rejected anonymous access.
    NeedsAuth { error: String },
}

impl McpStatus {
    /// Status name, matching the wire tag.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Pending => "pending",
            Self::Disabled => "disabled",
            Self::Failed { .. } => "failed",
            Self::NeedsAuth { .. } => "needs_auth",
        }
    }
}

/// `mcp.status.changed` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStatusEvent {
    pub server: String,
    pub status: McpStatus,
}

/// A catalog section that changed on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpChange {
    Tools,
    Prompts,
    Resources,
}

impl McpChange {
    /// Wire notification name.
    #[must_use]
    pub fn notification(self) -> &'static str {
        match self {
            Self::Tools => "notifications/tools/list_changed",
            Self::Prompts => "notifications/prompts/list_changed",
            Self::Resources => "notifications/resources/list_changed",
        }
    }
}

/// Server identity from `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

/// Client identity sent in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "opencode".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

impl ClientInfo {
    /// Default client identity.
    #[must_use]
    pub fn opencode() -> Self {
        Self::default()
    }
}

/// Handles server-initiated elicitation requests.
#[async_trait]
pub trait ElicitationHandler: Send + Sync + std::fmt::Debug {
    /// Presents `params` to the user and returns the elicitation result object.
    async fn create(&self, server: &str, params: Value) -> McpResult<Value>;
}

/// Options for one client connection.
#[derive(Clone)]
pub struct ClientOptions {
    /// Location directory exposed through `roots/list`.
    pub directory: PathBuf,
    /// Client identity.
    pub client_info: ClientInfo,
    /// Protocol negotiation mode.
    pub protocol: Protocol,
    /// Effective timeouts.
    pub timeouts: TimeoutConfig,
    /// Elicitation handler; its capabilities are advertised only when present.
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
    /// Status handler installed before the first `pending` event.
    pub status_handler: Option<Arc<dyn Fn(McpStatusEvent) + Send + Sync>>,
    /// Catalog change handler installed before connecting.
    pub change_handler: Option<Arc<dyn Fn(McpChange) + Send + Sync>>,
}

impl std::fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOptions")
            .field("directory", &self.directory)
            .field("client_info", &self.client_info)
            .field("protocol", &self.protocol)
            .field("timeouts", &self.timeouts)
            .field("elicitation", &self.elicitation.is_some())
            .field("status_handler", &self.status_handler.is_some())
            .field("change_handler", &self.change_handler.is_some())
            .finish()
    }
}

impl ClientOptions {
    /// Defaults for a location directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            client_info: ClientInfo::default(),
            protocol: Protocol::Legacy,
            timeouts: TimeoutConfig::default(),
            elicitation: None,
            status_handler: None,
            change_handler: None,
        }
    }
}

pub(super) type StatusHandler = Arc<dyn Fn(McpStatusEvent) + Send + Sync>;
pub(super) type ChangeHandler = Arc<dyn Fn(McpChange) + Send + Sync>;

/// `GET /api/mcp` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub status: McpStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "integrationID", skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
}
