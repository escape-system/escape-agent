//! Typed errors for the MCP client.
//!
//! Every failure path is represented here; malformed server output becomes
//! [`McpError::Protocol`] rather than a panic.

use std::time::Duration;

/// Result alias used across the crate.
pub type McpResult<T> = Result<T, McpError>;

/// A JSON-RPC error object sent by the server.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

/// Errors surfaced by the MCP client and transports.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    Config(String),
    #[error("MCP transport error: {0}")]
    Transport(String),
    #[error("MCP network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("MCP connection closed")]
    Closed,
    #[error("MCP server exited: {0}")]
    Exited(String),
    #[error("MCP request timed out after {0:?}")]
    Timeout(Duration),
    #[error("{0}")]
    Rpc(RpcError),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
    #[error("MCP server session expired")]
    SessionExpired,
    #[error("MCP server \"{server}\" needs authentication: {message}")]
    NeedsAuth { server: String, message: String },
    #[error("MCP server not found: {0}")]
    NotFound(String),
    #[error("MCP server \"{server}\" is not connected: {reason}")]
    NotConnected { server: String, reason: String },
    #[error("MCP tool \"{tool}\" on server \"{server}\" failed: {message}")]
    ToolCall {
        server: String,
        tool: String,
        message: String,
    },
    #[error("MCP tool \"{tool}\" on server \"{server}\" returned an error: {message}")]
    ToolReturnedError {
        server: String,
        tool: String,
        message: String,
    },
    #[error("MCP OAuth error: {0}")]
    OAuth(String),
    #[error("MCP stdio frame exceeded the 16 MiB limit")]
    FrameTooLarge,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl McpError {
    /// Whether a retry may succeed. Mirrors upstream: 408, 429 and 5xx
    /// (except 501) plus network failures.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Http { status, .. } => {
                *status == 408 || *status == 429 || (*status >= 500 && *status != 501)
            }
            Self::Network(_) => true,
            _ => false,
        }
    }

    /// `retry-after` advertised by an HTTP failure.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The JSON-RPC numeric code, when the error came from the peer.
    #[must_use]
    pub fn rpc_code(&self) -> Option<i64> {
        match self {
            Self::Rpc(error) => Some(error.code),
            _ => None,
        }
    }
}
