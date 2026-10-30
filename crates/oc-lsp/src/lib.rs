//! Language Server Protocol client for opencode-rust.
//!
//! The crate implements the runtime that upstream v2 only had as UI: a
//! JSON-RPC/stdio client, a bounded supervisor keyed by `(server, root)`,
//! push and pull diagnostics with a versioned LRU cache, and the `lsp` /
//! `lsp_diagnostics` tool operations described in `docs/11-lsp.md`.
//!
//! No language server is bundled and nothing is downloaded. Servers must
//! resolve on `PATH` or in the project's `node_modules/.bin`, or be configured
//! explicitly.

pub mod client;
pub mod config;
pub mod diagnostics;
pub mod error;
pub mod language;
pub mod position;
pub mod rpc;
pub mod supervisor;
pub mod tools;
pub mod uri;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use client::{
    DIAGNOSTICS_DEBOUNCE, DIAGNOSTICS_WAIT, INIT_TIMEOUT, LspClient, MAX_FILE_BYTES,
    REQUEST_TIMEOUT, SpawnRequest,
};
pub use config::{LspConfig, LspServerConfig, builtin_servers};
pub use diagnostics::{CachedDiagnostics, DiagnosticsCache};
pub use error::LspError;
pub use position::{Position, offset_to_position, position_to_offset, utf16_len};
pub use rpc::{RequestId, RpcError, RpcMessage};
pub use supervisor::{LspStatus, LspSupervisor, ServerKey, SupervisorOptions};
pub use tools::{
    EditDiagnostics, LspOperation, LspToolInput, WORKSPACE_SYMBOL_LIMIT, diagnostics,
    edit_diagnostics, execute, format_errors,
};
pub use uri::{is_file_uri, path_to_uri, uri_to_path};

/// Event name emitted when a client's cached diagnostics change.
pub const DIAGNOSTICS_EVENT: &str = "lsp.client.diagnostics";
/// Event name emitted when the registry/status changes.
pub const UPDATED_EVENT: &str = "lsp.updated";

/// Payload of `lsp.client.diagnostics`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspDiagnosticsEvent {
    /// Server that produced the diagnostics (`serverID` on the wire).
    #[serde(rename = "serverID")]
    pub server_id: String,
    /// Absolute path whose diagnostics changed.
    pub path: String,
}

/// Supervisor event, ready for the server's event bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspEvent {
    /// `lsp.updated` with an empty payload.
    Updated,
    /// `lsp.client.diagnostics`.
    Diagnostics(LspDiagnosticsEvent),
}

impl LspEvent {
    /// Wire event name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Updated => UPDATED_EVENT,
            Self::Diagnostics(_) => DIAGNOSTICS_EVENT,
        }
    }

    /// Wire payload.
    #[must_use]
    pub fn payload(&self) -> Value {
        match self {
            Self::Updated => json!({}),
            Self::Diagnostics(event) => serde_json::to_value(event).unwrap_or_else(|_| json!({})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_event_uses_the_upstream_field_name() {
        let event = LspDiagnosticsEvent {
            server_id: "rust".into(),
            path: "C:\\work\\a.rs".into(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["serverID"], "rust");
        assert!(value.get("serverId").is_none());
        let event2 = LspEvent::Diagnostics(event);
        assert_eq!(event2.name(), "lsp.client.diagnostics");
        assert_eq!(event2.payload()["serverID"], "rust");
        assert_eq!(LspEvent::Updated.name(), "lsp.updated");
        assert_eq!(LspEvent::Updated.payload(), json!({}));
    }
}
