//! Plugin hooks.
//!
//! # Envelope
//!
//! Hook events and transform documents are plain JSON documents. Neither live
//! Effect objects nor Rust values cross the native/JS boundary. The sidecar
//! wire envelope is:
//!
//! ```jsonc
//! // host → sidecar request
//! {"jsonrpc":"2.0","id":7,"method":"hook",
//!  "params":{"hook":"tool.execute.before","event":{...}}}
//! // sidecar → host response: the mutated event, or `null` when untouched
//! {"jsonrpc":"2.0","id":7,"result":{...}}
//! ```
//!
//! `Tool`/`Session`/`Permission` projections keep upstream field names and
//! leave unknown fields as [`serde_json::Value`] so a plugin can pass through
//! fields this crate does not model.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::Domain;
use crate::error::UnknownName;

/// A mutable hook event, carried as JSON.
pub type HookEvent = Value;

/// A mutable transform document, carried as JSON.
pub type TransformDoc = Value;

/// Hook points implemented by the runtime.
///
/// `aisdk.sdk`, `aisdk.language` and the `experimental.ws.*` hooks are not
/// bridged: they require live provider SDK objects / sockets (see
/// `docs/10-plugins.md`). The sidecar reports them as
/// `unsupported feature: <name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Hook {
    /// Before a tool call runs. The only hook that may abort the call.
    #[serde(rename = "tool.execute.before")]
    ToolExecuteBefore,
    /// After a tool call completed or failed.
    #[serde(rename = "tool.execute.after")]
    ToolExecuteAfter,
    /// Session prompt admission (text/files/skills).
    #[serde(rename = "session.prompt")]
    SessionPrompt,
    /// Model request context: system, messages, tools, options.
    #[serde(rename = "session.context")]
    SessionContext,
    /// Per-request base URL and headers.
    #[serde(rename = "session.model.request")]
    SessionModelRequest,
    /// Outbound provider HTTP request.
    #[serde(rename = "session.http.request")]
    SessionHttpRequest,
    /// Provider HTTP response.
    #[serde(rename = "session.http.response")]
    SessionHttpResponse,
    /// Retry decision.
    #[serde(rename = "session.retry")]
    SessionRetry,
    /// Session about to go idle; may admit synthetic input.
    #[serde(rename = "experimental.idle")]
    ExperimentalIdle,
    /// Permission evaluation.
    #[serde(rename = "permission.evaluate")]
    PermissionEvaluate,
    /// Before a shell command is created.
    #[serde(rename = "shell.create.before")]
    ShellCreateBefore,
}

impl Hook {
    /// Every implemented hook, in dispatch-documentation order.
    pub const ALL: [Self; 11] = [
        Self::ToolExecuteBefore,
        Self::ToolExecuteAfter,
        Self::SessionPrompt,
        Self::SessionContext,
        Self::SessionModelRequest,
        Self::SessionHttpRequest,
        Self::SessionHttpResponse,
        Self::SessionRetry,
        Self::ExperimentalIdle,
        Self::PermissionEvaluate,
        Self::ShellCreateBefore,
    ];

    /// Wire name (`tool.execute.before`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolExecuteBefore => "tool.execute.before",
            Self::ToolExecuteAfter => "tool.execute.after",
            Self::SessionPrompt => "session.prompt",
            Self::SessionContext => "session.context",
            Self::SessionModelRequest => "session.model.request",
            Self::SessionHttpRequest => "session.http.request",
            Self::SessionHttpResponse => "session.http.response",
            Self::SessionRetry => "session.retry",
            Self::ExperimentalIdle => "experimental.idle",
            Self::PermissionEvaluate => "permission.evaluate",
            Self::ShellCreateBefore => "shell.create.before",
        }
    }

    /// Whether a failure from this hook may abort the operation.
    ///
    /// Only `tool.execute.before` may fail; every other hook is infallible and
    /// a plugin error is logged and ignored.
    #[must_use]
    pub const fn can_fail(self) -> bool {
        matches!(self, Self::ToolExecuteBefore)
    }
}

impl fmt::Display for Hook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Hook {
    type Err = UnknownName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|hook| hook.as_str() == name)
            .ok_or_else(|| UnknownName {
                kind: "hook",
                name: name.to_string(),
            })
    }
}

/// Documented sidecar envelope for one hook call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEnvelope {
    /// The hook being dispatched.
    pub hook: Hook,
    /// The mutable event projection.
    pub event: HookEvent,
}

/// Documented sidecar envelope for one transform call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransformEnvelope {
    /// The document family being transformed.
    pub domain: Domain,
    /// The mutable document.
    pub doc: TransformDoc,
}
