//! Sidecar startup and transport failures.

/// Failure to start or drive the sidecar.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SidecarError {
    /// Neither a configured command nor `bun`/`node` is available.
    #[error("no JS runtime found: install bun or node")]
    RuntimeUnavailable,
    /// The process could not be spawned.
    #[error("failed to spawn plugin sidecar {command}: {message}")]
    Spawn {
        /// Command that was attempted.
        command: String,
        /// OS error text.
        message: String,
    },
    /// The `load` handshake exceeded the startup deadline.
    #[error("plugin sidecar did not start within {timeout_ms} ms")]
    StartupTimeout {
        /// Deadline in milliseconds.
        timeout_ms: u64,
    },
    /// The process exited, or flooded malformed frames.
    #[error("plugin sidecar crashed: {reason}")]
    Crashed {
        /// Crash reason.
        reason: String,
    },
    /// The sidecar rejected the load request.
    #[error("plugin sidecar failed to load plugins ({code}): {message}")]
    Load {
        /// JSON-RPC code.
        code: i64,
        /// Error message.
        message: String,
    },
}
