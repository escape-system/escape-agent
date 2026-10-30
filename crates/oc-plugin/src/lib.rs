//! Native plugin surface and JS sidecar bridge.
//!
//! Two plugin classes, exactly as described in `docs/10-plugins.md`:
//!
//! - **Native plugins** implement [`Plugin`] (or [`RpcNamespace`]) and are
//!   compiled in; [`PluginRegistry`] dispatches them in activation order
//!   (`builtin pre → sdk → instance → packages → builtin post`).
//! - **JS plugins** run in an external Bun/Node process managed by
//!   [`Sidecar`] over newline-framed JSON-RPC 2.0 stdio. No live Effect objects
//!   cross the bridge; hook events and transform documents are JSON with the
//!   envelopes documented in [`hook`] and [`sidecar::protocol`].
//!
//! No `unsafe`, no dynamic loading, and no npm execution happens in this crate.
//! The HTTP routes from `docs/10-plugins.md` are served elsewhere; this crate
//! provides the inventory shapes ([`PluginInfo`], [`PluginListEnvelope`]) and
//! dispatch entry points.
//!
//! # Unsupported upstream hooks
//!
//! `aisdk.sdk` / `aisdk.language` (live provider SDK objects, npm packages) and
//! the `experimental.ws.*` family (no WebSocket providers in this release) are
//! not bridged. The sidecar rejects a plugin that registers them with
//! `unsupported feature: <name>` in its inventory row.

pub mod domain;
pub mod error;
pub mod hook;
pub mod info;
pub mod plugin;
pub mod rpc;
pub mod sidecar;

pub use domain::Domain;
pub use error::{PluginError, UnknownName};
pub use hook::{Hook, HookEnvelope, HookEvent, TransformDoc, TransformEnvelope};
pub use info::{
    PLUGIN_UPDATED, PluginCheckRequest, PluginFeatures, PluginInfo, PluginListEnvelope,
    PluginSource, PluginState, PluginUpdateRequest, PluginUpdatedEvent,
};
pub use plugin::{Plugin, PluginRegistry, Tier, is_guarded_id};
pub use rpc::{RpcError, RpcErrorObject, RpcNamespace};
pub use sidecar::{
    HostServices, Sidecar, SidecarConfig, SidecarError, SidecarLog, SidecarPlugin, SidecarStatus,
    detect_runtime, detect_runtime_in,
};

/// Native + JS inventory, in dispatch order (`GET /api/plugin` `data`).
///
/// Native rows come first; the sidecar's plugins follow in registration order.
#[must_use]
pub fn plugin_inventory(registry: &PluginRegistry, sidecar: Option<&Sidecar>) -> Vec<PluginInfo> {
    let mut inventory = registry.inventory();
    if let Some(sidecar) = sidecar {
        inventory.extend(sidecar.inventory());
    }
    inventory
}
