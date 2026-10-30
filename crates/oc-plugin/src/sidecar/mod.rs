//! JS plugin sidecar bridge.
//!
//! One sidecar per server process, spawned lazily by the caller (the server
//! starts it only when `plugins` is non-empty or a plugin directory exists).
//! Transport is newline-framed JSON-RPC 2.0 over the child's stdin/stdout;
//! frames and envelopes are documented in [`protocol`].
//!
//! Lifecycle:
//!
//! - spawn (`plugin_host.command`, else `bun`, else `node`) → `load` with the
//!   configured targets, bounded by [`SidecarConfig::startup_timeout`] (5 s by
//!   default);
//! - every request (`hook`, `transform`, `rpc`, `reload`, `dispose`) is bounded
//!   by [`SidecarConfig::request_timeout`] (5 s by default);
//! - reader EOF or a write error marks the sidecar failed, fails pending
//!   requests and kills the process tree;
//! - `shutdown` sends `plugin.shutdown`, waits [`SidecarConfig::shutdown_grace`]
//!   (2 s) and then kills the process tree.
//!
//! The 90 MB RSS budget and crash-restart backoff from `docs/10-plugins.md` are
//! owned by the supervisor that calls this bridge; the bridge only detects and

mod config;
mod dispatch;
mod error;
mod inner;
mod process;
pub mod protocol;
mod register;
mod runtime;
mod status;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::{Mutex, RwLock};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

use crate::domain::Domain;
use crate::hook::Hook;
use crate::info::{PluginInfo, PluginState};
use crate::rpc::RpcError;

pub use config::SidecarConfig;
pub use error::SidecarError;
pub use runtime::{detect_runtime, detect_runtime_in};
pub use status::{HostServices, SidecarLog, SidecarPlugin, SidecarStatus};

use protocol::Exchange;

/// Malformed frames tolerated before the sidecar is considered failed.
const MAX_PROTOCOL_ERRORS: u32 = 3;
/// Log lines retained for the inventory/debug endpoint.
const MAX_LOGS: usize = 256;

struct Inner {
    config: SidecarConfig,
    command: String,
    status: Mutex<SidecarStatus>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Exchange, RpcError>>>>,
    outbound: mpsc::UnboundedSender<String>,
    child: tokio::sync::Mutex<Option<Child>>,
    plugins: Mutex<Vec<SidecarPlugin>>,
    logs: Mutex<VecDeque<SidecarLog>>,
    host: RwLock<Option<Arc<dyn HostServices>>>,
    protocol_errors: AtomicU32,
}

/// Handle to the JS plugin sidecar.
pub struct Sidecar {
    inner: Arc<Inner>,
}

impl fmt::Debug for Sidecar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sidecar")
            .field("command", &self.inner.command)
            .field("status", &self.status())
            .field("plugins", &self.inner.plugins.lock().len())
            .finish()
    }
}

impl Sidecar {
    /// Runtime command used by this sidecar.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.inner.command
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn status(&self) -> SidecarStatus {
        self.inner.status.lock().clone()
    }

    /// Whether the handshake completed and requests are served.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status().is_ready()
    }

    /// Registered plugins, in registration order.
    #[must_use]
    pub fn plugin_entries(&self) -> Vec<SidecarPlugin> {
        self.inner.plugins.lock().clone()
    }

    /// Log notifications received so far (bounded to 256).
    #[must_use]
    pub fn logs(&self) -> Vec<SidecarLog> {
        self.inner.logs.lock().iter().cloned().collect()
    }

    /// Number of malformed frames seen.
    #[must_use]
    pub fn protocol_errors(&self) -> u32 {
        self.inner.protocol_errors.load(Ordering::Relaxed)
    }

    /// Install the host service used by plugin callbacks. Replaces any previous
    /// one; callbacks are rejected until a host is installed.
    pub fn set_host(&self, host: Arc<dyn HostServices>) {
        *self.inner.host.write() = Some(host);
    }

    /// Whether any active plugin handles `hook`.
    #[must_use]
    pub fn has_hook(&self, hook: Hook) -> bool {
        self.inner
            .plugins
            .lock()
            .iter()
            .any(|plugin| plugin.state.is_active() && plugin.hooks.contains(&hook))
    }

    /// Whether any active plugin transforms `domain`.
    #[must_use]
    pub fn has_transform(&self, domain: Domain) -> bool {
        self.inner
            .plugins
            .lock()
            .iter()
            .any(|plugin| plugin.state.is_active() && plugin.transforms.contains(&domain))
    }

    /// Whether any active plugin exposes RPC.
    #[must_use]
    pub fn has_rpc(&self) -> bool {
        self.inner
            .plugins
            .lock()
            .iter()
            .any(|plugin| plugin.state.is_active() && plugin.features.rpc)
    }

    /// Inventory rows for the JS plugins.
    ///
    /// While the sidecar is failed or stopped, every row reports
    /// `failed (sidecar unavailable)` as required by the protocol.
    #[must_use]
    pub fn inventory(&self) -> Vec<PluginInfo> {
        let unavailable = !matches!(
            self.status(),
            SidecarStatus::Starting | SidecarStatus::Ready
        );
        self.inner
            .plugins
            .lock()
            .iter()
            .map(|plugin| PluginInfo {
                id: plugin.id.clone(),
                source: plugin.source.clone(),
                features: plugin.features,
                state: if unavailable {
                    PluginState::failed("sidecar unavailable")
                } else {
                    plugin.state.clone()
                },
            })
            .collect()
    }
}
