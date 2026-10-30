//! Plugin inventory (`Plugin.Info`) and the `plugin.updated` event.
//!
//! Wire shape mirrors `packages/schema/src/plugin.ts`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Event type emitted on every inventory change.
pub const PLUGIN_UPDATED: &str = "plugin.updated";

/// Where a plugin came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PluginSource {
    /// Compiled into the server.
    #[serde(rename = "builtin")]
    Builtin,
    /// An npm/git package loaded by the sidecar.
    #[serde(rename = "package")]
    Package {
        /// Original config target.
        target: String,
        /// Installed version, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        /// A newer version exists (only ever `true`).
        #[serde(default, skip_serializing_if = "is_not_true")]
        outdated: Option<bool>,
        /// An update is running (only ever `true`).
        #[serde(default, skip_serializing_if = "is_not_true")]
        updating: Option<bool>,
    },
    /// A local directory (`./rel`, absolute path, `file://`).
    #[serde(rename = "local")]
    Local {
        /// Absolute path of the plugin directory.
        path: String,
    },
    /// An SDK-embedded plugin.
    #[serde(rename = "sdk")]
    Sdk,
}

#[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
fn is_not_true(value: &Option<bool>) -> bool {
    *value != Some(true)
}

/// Entrypoints a plugin exposes. Absent features are omitted, never `false`.
// wire type: four independent `Literal(true)` optionals.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginFeatures {
    /// Has a server entrypoint.
    #[serde(default, skip_serializing_if = "is_false")]
    pub server: bool,
    /// Has a TUI entrypoint.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tui: bool,
    /// Registers at least one RPC namespace.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rpc: bool,
    /// Exports the desktop GUI bundle (`./gui-experimental`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub gui: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

impl PluginFeatures {
    /// A plain server-side plugin.
    #[must_use]
    pub const fn server() -> Self {
        Self {
            server: true,
            tui: false,
            rpc: false,
            gui: false,
        }
    }

    /// A server plugin that also serves RPC.
    #[must_use]
    pub const fn server_with_rpc() -> Self {
        Self {
            server: true,
            tui: false,
            rpc: true,
            gui: false,
        }
    }
}

/// Plugin health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PluginState {
    /// Loaded and dispatching.
    Active,
    /// Disabled, with the reason shown in the inventory.
    Failed {
        /// Why the plugin is not running.
        error: String,
        /// Optional reference (package id, file, generation).
        #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
    },
}

impl PluginState {
    /// The active state.
    #[must_use]
    pub const fn active() -> Self {
        Self::Active
    }

    /// A failed state with a reason.
    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
            r#ref: None,
        }
    }

    /// Whether the plugin dispatches.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// The failure reason, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Active => None,
            Self::Failed { error, .. } => Some(error),
        }
    }
}

/// One row of `GET /api/plugin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    /// Plugin id (package name, builtin id, or local entry name).
    pub id: String,
    /// Where it came from.
    pub source: PluginSource,
    /// Entrypoints it exposes.
    pub features: PluginFeatures,
    /// Whether it is active.
    pub state: PluginState,
}

impl PluginInfo {
    /// Whether the plugin is dispatching.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.state.is_active()
    }
}

/// `plugin.updated` event payload. Upstream emits it with an empty schema; the
/// location envelope is added by the server, not by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUpdatedEvent {
    /// Always [`PLUGIN_UPDATED`].
    #[serde(rename = "type")]
    pub event_type: &'static str,
}

impl PluginUpdatedEvent {
    /// The single event the plugin host emits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            event_type: PLUGIN_UPDATED,
        }
    }

    /// Serialize to the wire payload (`{"type":"plugin.updated"}`).
    #[must_use]
    pub fn to_value(self) -> Value {
        json!({ "type": self.event_type })
    }
}

impl Default for PluginUpdatedEvent {
    fn default() -> Self {
        Self::new()
    }
}

/// `GET /api/plugin` response envelope (`L`). `location` is passed through
/// verbatim from the caller's `Location.Info`; this crate owns no HTTP layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginListEnvelope {
    /// Location object, caller-supplied.
    pub location: Value,
    /// Inventory rows.
    pub data: Vec<PluginInfo>,
}

impl PluginListEnvelope {
    /// Wrap an inventory in the route envelope.
    #[must_use]
    pub fn new(location: Value, data: Vec<PluginInfo>) -> Self {
        Self { location, data }
    }
}

/// `{"target": "..."}` body of `POST /api/plugin/check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCheckRequest {
    /// One package target, or all when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// `{"targets": [...]}` body of `POST /api/plugin/update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUpdateRequest {
    /// Package targets to install/update.
    pub targets: Vec<String>,
}
