//! Typed policy errors.

use crate::model::{ModelCandidate, ModelSkip};

/// Why a spawn was refused. Every variant is decided before a session row is
/// written, so a caller can fail the tool call without partial state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    #[error("subagents are disabled")]
    Disabled,
    #[error("session {id} may not spawn: spawn_allowed is false")]
    SpawnNotAllowed { id: String },
    #[error("spawn depth {depth} exceeds max depth {max}")]
    DepthExceeded { depth: u32, max: u32 },
    #[error("session {id} already has {children} children (max {max})")]
    ChildLimit { id: String, children: u32, max: u32 },
    #[error("agent {agent} is not available for subagents")]
    UnknownAgent { agent: String },
    #[error("agent {agent} is primary-only and cannot be spawned")]
    PrimaryOnlyAgent { agent: String },
}

/// Why a `talk` message was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    #[error("session {id} is not in the session tree")]
    UnknownSender { id: String },
    #[error("session {id} is not in the session tree")]
    UnknownTarget { id: String },
    #[error("a session cannot message itself")]
    SelfMessage,
    #[error("sibling messaging is disabled")]
    SiblingsDisabled,
    #[error("sessions {from} and {to} are not siblings")]
    NotSibling { from: String, to: String },
    #[error("messaging the parent is disabled")]
    ParentDisabled,
    #[error("session {to} is outside the coordinator scope of {from}")]
    NotInScope { from: String, to: String },
}

/// Why a `session_write` was refused. Writing is stricter than messaging:
/// parents and (non-coordinator) siblings are never writable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("session {id} is not in the session tree")]
    UnknownSender { id: String },
    #[error("session {id} is not in the session tree")]
    UnknownTarget { id: String },
    #[error("session {to} is not a descendant of {from}")]
    NotDescendant { from: String, to: String },
    #[error("session {to} is outside the coordinator scope of {from}")]
    NotInScope { from: String, to: String },
}

/// Model resolution failure. Both variants carry the ordered candidate chain
/// so the tool error can show exactly what was tried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("no model candidates; supply a call/chain/agent/default/parent model")]
    NoCandidate { chain: Vec<ModelCandidate> },
    #[error(
        "no usable model candidate; skipped: {}; candidates: {}",
        skipped_display(skipped),
        chain_display(chain)
    )]
    NoUsableCandidate {
        chain: Vec<ModelCandidate>,
        skipped: Vec<ModelSkip>,
    },
}

/// `reference (reason, source), ...` in skip order. Never empty for this error:
/// a non-empty candidate list always yields at least one skip.
fn skipped_display(skipped: &[ModelSkip]) -> String {
    if skipped.is_empty() {
        return "none".to_owned();
    }
    skipped
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `reference (source), ...` in resolution order.
fn chain_display(chain: &[ModelCandidate]) -> String {
    if chain.is_empty() {
        return "none".to_owned();
    }
    chain
        .iter()
        .map(|candidate| format!("{} ({})", candidate.reference, candidate.source))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Spawn/message rate-limit failure. Times are explicit milliseconds, so the
/// policy stays pure and testable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    #[error("session spawn limit reached (max {max})")]
    SessionLimit { max: u32 },
    #[error("rate limit reached (max {max_per_minute}/min); retry after {retry_after_ms} ms")]
    RateLimited {
        max_per_minute: u32,
        retry_after_ms: i64,
    },
}
