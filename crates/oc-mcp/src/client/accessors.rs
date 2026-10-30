//! MCP client accessors and handler installation.

use std::sync::Arc;

use serde_json::Value;

use super::McpClient;
use super::lock;
use super::types::{McpChange, McpServerInfo, McpStatus, McpStatusEvent, ServerInfo};

impl McpClient {
    /// Server name from config.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.inner.server
    }

    /// Child process id for a stdio server.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.inner.current_transport().pid()
    }

    /// Session id minted by an HTTP server.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.inner.current_transport().session_id()
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> McpStatus {
        lock(&self.inner.state)
            .status
            .clone()
            .unwrap_or(McpStatus::Pending)
    }

    /// `GET /api/mcp` entry for this client.
    #[must_use]
    pub fn info(&self) -> McpServerInfo {
        let title = lock(&self.inner.state)
            .server_info
            .as_ref()
            .and_then(|info| info.title.clone());
        McpServerInfo {
            name: self.inner.server.clone(),
            status: self.status(),
            title,
            integration_id: None,
        }
    }

    /// Server identity from `initialize`.
    #[must_use]
    pub fn server_info(&self) -> Option<ServerInfo> {
        lock(&self.inner.state).server_info.clone()
    }

    /// Server instructions.
    #[must_use]
    pub fn instructions(&self) -> Option<String> {
        lock(&self.inner.state).instructions.clone()
    }

    /// Negotiated protocol revision.
    #[must_use]
    pub fn protocol_version(&self) -> Option<String> {
        lock(&self.inner.state).protocol_version.clone()
    }

    /// Advertised capabilities.
    #[must_use]
    pub fn capabilities(&self) -> Value {
        lock(&self.inner.state).capabilities.clone()
    }

    /// Installs a status change handler.
    pub fn set_status_handler(&self, handler: Arc<dyn Fn(McpStatusEvent) + Send + Sync>) {
        *self
            .inner
            .status_handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    /// Installs a catalog change handler.
    pub fn set_change_handler(&self, handler: Arc<dyn Fn(McpChange) + Send + Sync>) {
        *self
            .inner
            .change_handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    pub(super) fn supports(&self, capability: &str) -> bool {
        lock(&self.inner.state)
            .capabilities
            .get(capability)
            .is_some_and(|value| !value.is_null())
    }
}
