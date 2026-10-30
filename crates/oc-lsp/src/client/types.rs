//! Client timeouts and the spawn request.

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::broadcast;

use crate::LspEvent;
use crate::config::LspServerConfig;

/// `initialize` must complete within this window.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(45);
/// Interactive requests time out after this long.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Files above this size are not synchronized.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Diagnostics events are coalesced by this delay.
pub const DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(150);
/// `edit`/`write` wait this long for diagnostics.
pub const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);

/// Everything needed to start one client.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Server definition (id, env, initialization).
    pub server: LspServerConfig,
    /// Canonical project root.
    pub root: PathBuf,
    /// Resolved argv (absolute executable first).
    pub argv: Vec<String>,
    /// Event fan-out shared by all clients.
    pub events: broadcast::Sender<LspEvent>,
    /// Initialize timeout.
    pub init_timeout: Duration,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Diagnostics wait budget.
    pub diagnostics_wait: Duration,
}
