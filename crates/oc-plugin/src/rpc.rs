//! Typed RPC namespaces and errors.
//!
//! Native plugins may expose one namespace each; the JS sidecar exposes the
//! namespaces its plugins register. The HTTP layer mounts dispatch at
//! `POST /api/rpc/:namespace/:method` (see `docs/10-plugins.md`); this crate
//! only performs the dispatch and returns typed errors.
//!
//! JSON-RPC error codes used on the wire:
//!
//! | Code | Meaning |
//! |---|---|
//! | `-32601` | unknown namespace or method |
//! | `-32602` | invalid input |
//! | `-32000` | plugin failure |
//! | `-32001` | timeout |
//! | `-32002` | sidecar unavailable |

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

/// A namespace of methods served by one plugin.
///
/// A namespace is owned by exactly one plugin; a second plugin declaring the
/// same namespace is marked failed, so it can never intercept the first one's
/// calls.
pub trait RpcNamespace: Send + Sync {
    /// The namespace segment (`/api/rpc/<namespace>/<method>`).
    fn namespace(&self) -> &str;

    /// Dispatch one method. Implementations return
    /// [`RpcError::MethodNotFound`] for unknown methods and
    /// [`RpcError::InvalidInput`] for schema violations.
    fn invoke(&self, method: &str, input: Value) -> Result<Value, RpcError>;
}

/// Typed RPC failure.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum RpcError {
    /// No active plugin serves the namespace.
    #[error("unknown rpc namespace: {namespace}")]
    NamespaceNotFound {
        /// Requested namespace.
        namespace: String,
    },
    /// The namespace exists but has no such method.
    #[error("unknown rpc method: {namespace}.{method}")]
    MethodNotFound {
        /// Requested namespace.
        namespace: String,
        /// Requested method.
        method: String,
    },
    /// The request input did not match the method's schema.
    #[error("invalid rpc input: {message}")]
    InvalidInput {
        /// What was wrong with the input.
        message: String,
    },
    /// The owning plugin failed. May carry plugin-supplied data.
    #[error("plugin rpc failed: {message}")]
    Failed {
        /// Human readable failure.
        message: String,
        /// Plugin-supplied error data.
        data: Option<Value>,
    },
    /// The sidecar did not answer within the deadline.
    #[error("rpc {method} timed out after {timeout_ms} ms")]
    Timeout {
        /// Method that timed out.
        method: String,
        /// Deadline in milliseconds.
        timeout_ms: u64,
    },
    /// The sidecar is not running.
    #[error("sidecar unavailable: {message}")]
    Unavailable {
        /// Why the sidecar cannot serve the call.
        message: String,
    },
}

impl RpcError {
    /// JSON-RPC error code.
    #[must_use]
    pub const fn code(&self) -> i64 {
        match self {
            Self::NamespaceNotFound { .. } | Self::MethodNotFound { .. } => -32601,
            Self::InvalidInput { .. } => -32602,
            Self::Failed { .. } => -32000,
            Self::Timeout { .. } => -32001,
            Self::Unavailable { .. } => -32002,
        }
    }

    /// Wire object, optional `data` omitted (never `null`).
    #[must_use]
    pub fn to_object(&self) -> RpcErrorObject {
        match self {
            Self::NamespaceNotFound { namespace } => RpcErrorObject {
                code: self.code(),
                message: self.to_string(),
                data: Some(json!({ "namespace": namespace })),
            },
            Self::MethodNotFound { namespace, method } => RpcErrorObject {
                code: self.code(),
                message: self.to_string(),
                data: Some(json!({ "namespace": namespace, "method": method })),
            },
            Self::InvalidInput { .. } | Self::Unavailable { .. } => RpcErrorObject {
                code: self.code(),
                message: self.to_string(),
                data: None,
            },
            Self::Failed { data, .. } => RpcErrorObject {
                code: self.code(),
                message: self.to_string(),
                data: data.clone(),
            },
            Self::Timeout { method, timeout_ms } => RpcErrorObject {
                code: self.code(),
                message: self.to_string(),
                data: Some(json!({ "method": method, "timeoutMs": timeout_ms })),
            },
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorObject {
    /// Numeric error code.
    pub code: i64,
    /// Human readable message.
    pub message: String,
    /// Optional plugin-supplied data; omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
