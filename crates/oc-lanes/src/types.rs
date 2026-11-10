//! Public lane values shared with the runtime, the shell tool and the UI.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A request to run one shell command inside a resource lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewTicket {
    /// The exact command string the runner should execute.
    pub command: String,
    /// Working directory of the command.
    pub cwd: PathBuf,
    /// Higher values are dispatched first. Equal priorities are FIFO.
    pub priority: i32,
    /// Session that submitted the command, when known.
    pub owner_session: Option<String>,
    /// Optional display label. It is not part of the public [`Ticket`].
    pub label: Option<String>,
    /// Opaque caller-supplied tree state (for example a git revision plus a
    /// dirty-tree hash). It is never interpreted by the service.
    ///
    /// Two submissions coalesce only when this is `Some(non-empty)`, the lane,
    /// the normalized command and the working directory match, and the ticket
    /// is still queued or running. [`None`] always creates a distinct ticket:
    /// reusing a run that started from a different tree state would silently
    /// serve a stale result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_rev: Option<String>,
}

/// Opaque ticket identifier.
pub type TicketId = String;

/// Outcome of [`crate::LaneService::enqueue`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnqueueResult {
    /// Id of the ticket that runs the command.
    pub id: TicketId,
    /// `true` when an identical active ticket was reused instead of creating a
    /// new one. The caller waits on [`Self::id`] exactly like its own ticket.
    pub subscribed: bool,
}

/// One persisted lane ticket, as replayed by [`crate::LaneService::restore`].
///
/// It is the wire [`Ticket`] shape without the coalescing-only fields
/// (`tree_rev`, `subscribers`), including the persisted terminal fields, so a
/// JSON row from `oc_store::Store::lane_tickets_all` deserializes directly and
/// a restored history row is faithful to what the store holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketRecord {
    /// Persisted ticket id (`tkt_<n>`).
    pub id: String,
    /// Lane that owns the ticket.
    pub lane: String,
    /// Command string.
    pub command: String,
    /// Working directory of the command.
    pub cwd: PathBuf,
    /// Dispatch priority.
    pub priority: i32,
    /// Session that submitted the command, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session: Option<String>,
    /// Last persisted state.
    pub state: TicketState,
    /// Time the ticket entered the queue (epoch milliseconds).
    pub queued_ms: i64,
    /// Time a runner started the ticket, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<i64>,
    /// Time the ticket reached a terminal state, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<i64>,
    /// File that holds the captured command output, when the runner wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
}

/// Lifecycle of a ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TicketState {
    /// Waiting in the lane queue.
    Queued,
    /// A runner is executing the command.
    Running,
    /// The command finished with exit code zero.
    Succeeded {
        /// Process exit code (always zero for this variant).
        exit: i32,
    },
    /// The command finished with a non-zero exit code or the runner failed.
    Failed {
        /// Process exit code, or the runner's synthetic code.
        exit: i32,
        /// Human-readable failure reason.
        message: String,
    },
    /// The ticket was cancelled before or during execution.
    Cancelled,
}

impl TicketState {
    /// Whether no further transition is possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }
}

/// A queued, running or finished lane ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    /// Stable id, also used as the cancellation-token key.
    pub id: String,
    /// Lane that owns the ticket.
    pub lane: String,
    /// Command string submitted by the caller.
    pub command: String,
    /// Working directory of the command.
    pub cwd: PathBuf,
    /// Current dispatch priority.
    pub priority: i32,
    /// Session that submitted the command, when known.
    pub owner_session: Option<String>,
    /// Current state.
    pub state: TicketState,
    /// Time the ticket entered the queue (epoch milliseconds).
    pub queued_ms: i64,
    /// Time a runner started the ticket (epoch milliseconds).
    pub started_ms: Option<i64>,
    /// Time the ticket reached a terminal state (epoch milliseconds).
    pub finished_ms: Option<i64>,
    /// File the runner captured command output to, when the runner reports
    /// one. It is copied from [`RunnerResult::output_path`] on success and
    /// failure; a cancelled ticket never gains one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
    /// Tree state the ticket was coalesced on, when the caller supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_rev: Option<String>,
    /// Sessions that subscribed to this ticket through a coalesced
    /// [`crate::LaneService::enqueue`], deduplicated, in subscription order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscribers: Vec<String>,
}

/// Snapshot of one lane used by `lane.status` and the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneStatus {
    /// Lane name (`cargo:<workspace>`, `network`, …).
    pub lane: String,
    /// Resolved concurrency.
    pub concurrency: usize,
    /// Runner slots currently occupied.
    pub running: usize,
    /// Tickets waiting in the priority queue.
    pub queued: usize,
}

/// Result reported by a [`crate::LaneRunner`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerResult {
    /// Process exit code. Zero is success; any other value becomes
    /// [`TicketState::Failed`].
    pub exit: i32,
    /// Failure reason, used when `exit` is non-zero.
    pub message: Option<String>,
    /// File that holds the captured command output, when the runner wrote one.
    /// The service copies it onto the finished [`Ticket`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<std::path::PathBuf>,
}

impl RunnerResult {
    /// A successful result.
    pub const fn success() -> Self {
        Self {
            exit: 0,
            message: None,
            output_path: None,
        }
    }

    /// A failed result with a reason.
    pub fn failure(exit: i32, message: impl Into<String>) -> Self {
        Self {
            exit,
            message: Some(message.into()),
            output_path: None,
        }
    }

    /// Attaches the file that captured the command output.
    #[must_use]
    pub fn with_output_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.output_path = Some(path.into());
        self
    }
}

/// Result of [`crate::classify`]: the lane a command belongs to and the
/// original command string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRequest {
    /// Resolved lane name.
    pub lane: String,
    /// The command string, unchanged.
    pub command: String,
}
