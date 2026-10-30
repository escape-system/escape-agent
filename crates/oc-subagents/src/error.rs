//! Typed policy errors.

use crate::model::ModelCandidate;

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

/// Model resolution failure. The chain lists every candidate in priority
/// order so the tool error can show what was tried.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("no model candidates; supply a call/agent/default/parent model")]
    NoCandidate,
    #[error("model reference {reference} ({origin}) is malformed; candidates: {chain:?}")]
    InvalidReference {
        reference: String,
        origin: &'static str,
        chain: Vec<ModelCandidate>,
    },
    #[error("model {reference} ({origin}) is unavailable; candidates: {chain:?}")]
    Unavailable {
        reference: String,
        origin: &'static str,
        chain: Vec<ModelCandidate>,
    },
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
