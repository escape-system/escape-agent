//! Client accessors: identity, capabilities, counters and process state.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde_json::Value;

use super::LspClient;

impl LspClient {
    /// Server id from configuration.
    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    /// Human-readable server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.inner.server_name
    }

    /// Canonical root this client serves.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Capabilities returned by `initialize`.
    #[must_use]
    pub fn capabilities(&self) -> Value {
        self.inner.capabilities.lock().clone()
    }

    /// Whether the server advertised `textDocument/diagnostic`.
    #[must_use]
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.inner.pull_supported.load(Ordering::Acquire)
    }

    /// Outstanding requests.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Relaxed)
    }

    /// When the client was last used by a tool.
    #[must_use]
    pub fn last_used(&self) -> Instant {
        *self.inner.last_used.lock()
    }

    /// The child process is gone.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        self.inner.exited.load(Ordering::Acquire)
    }

    /// Last bytes written by the server to stderr, for error reporting.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.inner.stderr.lock().clone()
    }

    /// Child process id, when the platform exposes one.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.inner.pid
    }
}
