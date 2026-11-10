//! Lane errors returned to the shell and coordinator tools.

/// A lane operation that could not be completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaneError {
    /// The lane's queue is full.
    #[error("lane `{lane}` queue is full (max {max})")]
    QueueFull {
        /// Lane name.
        lane: String,
        /// Configured maximum queue length.
        max: usize,
    },
    /// No ticket with this id exists.
    #[error("unknown ticket `{0}`")]
    UnknownTicket(String),
    /// Only queued tickets can be reordered or cancelled as queued.
    #[error("ticket `{id}` is not queued")]
    NotQueued {
        /// Ticket id.
        id: String,
    },
    /// The service has been shut down; no new work is accepted.
    #[error("lane service is shut down")]
    Shutdown,
    /// An empty lane name was rejected.
    #[error("lane name must not be empty")]
    EmptyLane,
    /// `wait` reached its deadline before the ticket became terminal.
    #[error("ticket `{id}` did not finish within {timeout_ms} ms")]
    Timeout {
        /// Ticket id.
        id: String,
        /// Deadline in milliseconds.
        timeout_ms: u64,
    },
}

/// Why [`crate::LaneService::restore`] could not replay persisted tickets.
///
/// Boot-only: no route handler returns this, so it is deliberately separate
/// from [`LaneError`] and every existing `LaneError` match stays exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestoreError {
    /// The persisted ticket source failed (store read or row decode).
    #[error("cannot load persisted lane tickets: {0}")]
    Source(String),
    /// A persisted row cannot be replayed without corrupting the queue.
    #[error("lane ticket `{id}` cannot be restored: {reason}")]
    Ticket {
        /// Ticket id.
        id: String,
        /// Human-readable reason.
        reason: String,
    },
}
