//! Externally visible observer signals: notices, run results, skip reasons and
//! host execution flags, plus the repair result the host returns.

use serde::{Deserialize, Serialize};

use crate::goal::{GoalSnapshot, GoalStatus};
use crate::verdict::{ObserverAction, ObserverVerdict, RepairKind};

use super::budget::BudgetState;

/// Result of applying a repair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    /// Whether the host accepted and performed the repair.
    pub applied: bool,
    /// Optional human-readable detail.
    pub detail: Option<String>,
}

/// Externally visible observer events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverNotice {
    /// A repair was applied.
    Repaired {
        /// Root session.
        root_id: String,
        /// Repair target.
        target: String,
        /// Repair kind.
        kind: RepairKind,
        /// Observer reason.
        reason: String,
        /// 1-based attempt within the window.
        attempt: u32,
        /// Remaining root budget.
        budget_left: u32,
    },
    /// The repair budget is exhausted; the observer stops acting.
    Escalated {
        /// Root session.
        root_id: String,
        /// Why escalation happened.
        reason: String,
        /// Budget counters at escalation time.
        budget: BudgetState,
    },
    /// The observer model failed or produced an invalid verdict.
    Unavailable {
        /// Root session.
        root_id: String,
        /// Failure detail.
        reason: String,
    },
    /// A nudge was requested (the host injects the synthetic).
    Nudged {
        /// Root session.
        root_id: String,
        /// Target session.
        target: String,
        /// Nudge text.
        message: String,
    },
    /// An inspect/digest was requested.
    Inspected {
        /// Root session.
        root_id: String,
        /// Target session or tool call.
        target: String,
        /// Observer reason.
        reason: String,
    },
    /// The host accepted a repair but failed to execute it.
    RepairFailed {
        /// Root session.
        root_id: String,
        /// Repair target.
        target: String,
        /// Repair kind.
        kind: RepairKind,
        /// Failure detail.
        reason: String,
    },
    /// Goal state changed (or was rejected). The host persists `goal`.
    Goal {
        /// Root session.
        root_id: String,
        /// Goal id.
        goal_id: String,
        /// New status.
        status: GoalStatus,
        /// Transition explanation.
        reason: String,
        /// Full post-verdict snapshot for the `GoalStore`.
        goal: GoalSnapshot,
    },
}

/// Why a tick did not call the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Within `min_interval_ms` of the previous call.
    Coalesced,
    /// `max_calls_per_min` reached.
    RateLimited,
}

/// Result of one observer tick.
#[derive(Debug, Clone, PartialEq)]
pub enum ObserverRun {
    /// A non-repair verdict was produced.
    Verdict {
        /// Validated verdict.
        verdict: ObserverVerdict,
    },
    /// A repair was produced and handed to the executor.
    Repair {
        /// Validated verdict.
        verdict: ObserverVerdict,
        /// Executor result.
        outcome: RepairOutcome,
    },
    /// The model was not called because of coalescing.
    Skipped {
        /// Coalescing reason.
        reason: SkipReason,
    },
    /// The verdict was refused by a guard (budget, idempotency, …).
    Rejected {
        /// Refused action.
        action: ObserverAction,
        /// Human-readable reason.
        reason: String,
    },
    /// Repair budget is exhausted; no further automatic repairs.
    Escalated {
        /// Why.
        reason: String,
        /// Budget state.
        budget: BudgetState,
    },
    /// Model failure or invalid verdict: fail open, no repair.
    Unavailable {
        /// Failure detail.
        reason: String,
    },
}

/// Host execution signals that wake the observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostFlag {
    /// A session execution failed.
    ExecutionFailed,
    /// A session execution was interrupted.
    ExecutionInterrupted,
    /// A session execution was interrupted because the user pressed stop.
    ///
    /// This is terminal for the observer: it must not wake, nudge, repost or
    /// restart the stopped session until a new execution starts.
    ExecutionInterruptedByUser,
    /// A session execution started (clears a previous user stop).
    ExecutionStarted,
    /// A step completed (activity, not a wake on its own).
    StepEvent,
}
