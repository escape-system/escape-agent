//! Configuration for the observer and the goal supervisor.
//!
//! Defaults match `docs/08-goals-observer.md` §7. Every field has a default so
//! a host may deserialize a partial `observer` object.

use serde::{Deserialize, Serialize};

/// Observer loop and repair configuration (`observer.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObserverConfig {
    /// When false the engine never calls the model.
    pub enabled: bool,
    /// Observer model reference (`provider/id`).
    pub model: Option<String>,
    /// Model used by `digest`/`inspect`.
    pub digest_model: Option<String>,
    /// Model offered to `repair.switch_model`; when absent the kind is rejected.
    pub escalation_model: Option<String>,
    /// Actor tick hint in milliseconds.
    pub tick_ms: u64,
    /// Minimum gap between two model calls for one root.
    pub min_interval_ms: i64,
    /// Sliding one-minute call ceiling per root.
    pub max_calls_per_min: u32,
    /// Hard input budget; rendered JSON is capped at `max_input_tokens * 4`
    /// characters.
    pub max_input_tokens: usize,
    /// Busy execution with no activity for this long is a stall.
    pub stall_seconds: i64,
    /// Repair ceiling per root per ten minutes.
    pub max_repairs_per_10min: u32,
    /// Repair ceiling per target per ten minutes.
    pub max_repairs_per_target_10min: u32,
    /// Hard cap for a nudge `message`.
    pub nudge_max_chars: usize,
    /// Nudges per target per ten minutes.
    pub max_nudges_per_target_10min: u32,
    /// Inspects per root per ten minutes.
    pub max_inspects_per_10min: u32,
    /// `compact_context` cooldown per target.
    pub compact_cooldown_ms: i64,
    /// Sliding window for repair/nudge/inspect budgets.
    pub budget_window_ms: i64,
    /// Minimum gap between `observer.unavailable` notices per root.
    pub unavailable_notice_interval_ms: i64,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: None,
            digest_model: None,
            escalation_model: None,
            tick_ms: 1_000,
            min_interval_ms: 2_000,
            max_calls_per_min: 12,
            max_input_tokens: 600,
            stall_seconds: 120,
            max_repairs_per_10min: 5,
            max_repairs_per_target_10min: 2,
            nudge_max_chars: 400,
            max_nudges_per_target_10min: 2,
            max_inspects_per_10min: 6,
            compact_cooldown_ms: 600_000,
            budget_window_ms: 600_000,
            unavailable_notice_interval_ms: 300_000,
        }
    }
}

/// Goal supervisor configuration (`goal.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalConfig {
    /// When false the goal tools are not exposed by the host.
    pub enabled: bool,
    /// Round ceiling before the goal fails.
    pub max_rounds: u32,
    /// Consecutive no-progress rounds before the goal fails.
    pub max_stalls: u32,
    /// Total wait budget before a waiting goal blocks.
    pub wait_limit_minutes: u32,
}

impl Default for GoalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rounds: 50,
            max_stalls: 3,
            wait_limit_minutes: 30,
        }
    }
}
