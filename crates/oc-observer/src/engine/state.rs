//! Per-root observer bookkeeping plus the budget-window pruning that resets it.

use std::collections::HashMap;

use crate::config::ObserverConfig;
use crate::goal::GoalStatus;
use crate::input::RootContext;

/// Mutable per-root observer history.
#[derive(Debug, Default)]
pub(crate) struct RootState {
    pub(super) root_context: RootContext,
    pub(super) last_call_ms: Option<i64>,
    pub(super) calls: Vec<i64>,
    pub(super) last_activity_ms: Option<i64>,
    pub(super) error_streak: u32,
    pub(super) pending_wake: bool,
    /// True between a user stop and the next execution start.
    ///
    /// Terminal for the observer: no tick, nudge, repost or restart until the
    /// root runs again.
    pub(super) user_stopped: bool,
    pub(super) last_goal: Option<(GoalStatus, u32)>,
    pub(super) escalated: bool,
    pub(super) last_unavailable_notice_ms: Option<i64>,
    pub(super) repairs: Vec<i64>,
    pub(super) target_repairs: HashMap<String, Vec<i64>>,
    pub(super) nudges: HashMap<String, Vec<i64>>,
    pub(super) inspects: Vec<i64>,
    pub(super) compacts: HashMap<String, i64>,
    /// Overdue strips already evaluated, keyed by tool-call id, with the
    /// **monotonic** time ([`crate::clock::Clock::mono_ms`]) before which the
    /// same strip must not wake the observer again.
    ///
    /// Without this an overdue strip stays overdue on every tick, so one long
    /// `subagent` or `shell` call re-woke the model every `min_interval_ms`
    /// for as long as it ran.
    ///
    /// The deadline is monotonic on purpose: a backward wall-clock jump must
    /// not suppress a genuinely overdue strip.
    pub(super) snoozed: HashMap<String, i64>,
}

/// Minimum re-evaluation backoff for an overdue strip.
pub(super) const MIN_OVERDUE_BACKOFF_MS: i64 = 30_000;

pub(super) fn prune_all(state: &mut RootState, now: i64, cfg: &ObserverConfig) {
    prune_vec(&mut state.calls, now, 60_000);
    prune_vec(&mut state.repairs, now, cfg.budget_window_ms);
    for repairs in state.target_repairs.values_mut() {
        prune_vec(repairs, now, cfg.budget_window_ms);
    }
    for nudges in state.nudges.values_mut() {
        prune_vec(nudges, now, cfg.budget_window_ms);
    }
    prune_vec(&mut state.inspects, now, cfg.budget_window_ms);
    state
        .target_repairs
        .retain(|_, repairs| !repairs.is_empty());
    state.nudges.retain(|_, nudges| !nudges.is_empty());
    // Escalation is a budget state: it ends when the window rolls over.
    if state.escalated
        && u32::try_from(state.repairs.len()).unwrap_or(u32::MAX) < cfg.max_repairs_per_10min
    {
        state.escalated = false;
    }
}

pub(super) fn prune_vec(values: &mut Vec<i64>, now: i64, window_ms: i64) {
    values.retain(|value| now.saturating_sub(*value) < window_ms);
}
