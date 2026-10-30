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
}

/// Opaque ticket identifier.
pub type TicketId = String;

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
