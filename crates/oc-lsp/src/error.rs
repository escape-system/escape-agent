//! Error types shared by the LSP client, supervisor and tool facade.

use std::path::PathBuf;

use crate::rpc::RpcError;

/// Errors surfaced by the LSP subsystem.
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    /// No configured server matches the file.
    #[error("no LSP server available for file: {0}")]
    NoServer(PathBuf),

    /// A server matched the file but its command is not installed or is
    /// disabled.
    #[error("LSP server `{server}` is not available: {reason}")]
    Unavailable { server: String, reason: String },

    /// The server is in exponential backoff after a crash.
    #[error("LSP server `{server}` is in backoff after {failures} failure(s): {reason}")]
    Backoff {
        server: String,
        failures: u32,
        reason: String,
    },

    /// Spawning the child process failed.
    #[error("failed to start LSP server `{server}`: {message}")]
    Spawn {
        server: String,
        message: String,
        /// A missing binary is not retried until the configuration changes.
        missing: bool,
    },

    /// The server did not answer in time.
    #[error("LSP request `{method}` timed out after {millis} ms")]
    Timeout { method: String, millis: u64 },

    /// The server answered with a JSON-RPC error.
    #[error("LSP server error ({code}): {message}")]
    Server { code: i64, message: String },

    /// The operation is deliberately not supported in v1.
    #[error("unsupported operation: {0}")]
    Unsupported(String),

    /// File is outside the workspace root.
    #[error("file is outside the workspace root: {0}")]
    OutsideRoot(PathBuf),

    /// The per-supervisor memory budget cannot satisfy another client.
    #[error("LSP server budget exceeded (max {max} clients, {slots} slots)")]
    BudgetExceeded { max: usize, slots: usize },

    /// LSP is turned off in configuration.
    #[error("LSP is disabled")]
    Disabled,

    /// Tool input is malformed.
    #[error("invalid LSP arguments: {0}")]
    InvalidParams(String),

    /// Transport failure.
    #[error(transparent)]
    Rpc(#[from] RpcError),

    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl LspError {
    /// Rebuild an equivalent error from a shared reference.
    ///
    /// Shared spawn futures hand out `Arc<LspError>`; callers need an owned
    /// value. `std::io::Error` and `serde_json::Error` are not `Clone`, so
    /// those two are re-created from their kind/message.
    #[must_use]
    pub fn duplicate(&self) -> Self {
        match self {
            Self::NoServer(path) => Self::NoServer(path.clone()),
            Self::Unavailable { server, reason } => Self::Unavailable {
                server: server.clone(),
                reason: reason.clone(),
            },
            Self::Backoff {
                server,
                failures,
                reason,
            } => Self::Backoff {
                server: server.clone(),
                failures: *failures,
                reason: reason.clone(),
            },
            Self::Spawn {
                server,
                message,
                missing,
            } => Self::Spawn {
                server: server.clone(),
                message: message.clone(),
                missing: *missing,
            },
            Self::Timeout { method, millis } => Self::Timeout {
                method: method.clone(),
                millis: *millis,
            },
            Self::Server { code, message } => Self::Server {
                code: *code,
                message: message.clone(),
            },
            Self::Unsupported(operation) => Self::Unsupported(operation.clone()),
            Self::OutsideRoot(path) => Self::OutsideRoot(path.clone()),
            Self::BudgetExceeded { max, slots } => Self::BudgetExceeded {
                max: *max,
                slots: *slots,
            },
            Self::Disabled => Self::Disabled,
            Self::InvalidParams(message) => Self::InvalidParams(message.clone()),
            Self::Io(error) => Self::Io(std::io::Error::new(error.kind(), error.to_string())),
            Self::Rpc(error) => match error {
                RpcError::Io(io) => Self::Io(std::io::Error::new(io.kind(), io.to_string())),
                RpcError::Timeout(method) => Self::Rpc(RpcError::Timeout(method.clone())),
                RpcError::Server { code, message } => Self::Server {
                    code: *code,
                    message: message.clone(),
                },
                other => Self::InvalidParams(format!("transport: {other}")),
            },
        }
    }

    /// `true` when a retry without a configuration change cannot succeed.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::NoServer(_)
                | Self::Unavailable { .. }
                | Self::Spawn { missing: true, .. }
                | Self::Disabled
                | Self::Unsupported(_)
        )
    }
}
