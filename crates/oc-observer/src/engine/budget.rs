//! Repair budget accounting: the per-root and per-target snapshot plus the
//! derivation of that snapshot from [`RootState`].

use serde::{Deserialize, Serialize};

use crate::config::ObserverConfig;

use super::state::RootState;

/// Per-target repair usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetBudget {
    /// Target id.
    pub target: String,
    /// Repairs in the current window.
    pub used: u32,
    /// Per-target limit.
    pub limit: u32,
}

/// Repair budget snapshot for one root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetState {
    /// Repairs used in the current window.
    pub root_used: u32,
    /// Per-root limit.
    pub root_limit: u32,
    /// Per-target usage.
    pub targets: Vec<TargetBudget>,
    /// True when no further repair may run automatically.
    pub exhausted: bool,
}

pub(super) fn budget_from(state: &RootState, cfg: &ObserverConfig) -> BudgetState {
    let root_used = u32::try_from(state.repairs.len()).unwrap_or(u32::MAX);
    let mut targets: Vec<TargetBudget> = state
        .target_repairs
        .iter()
        .filter(|(_, repairs)| !repairs.is_empty())
        .map(|(target, repairs)| TargetBudget {
            target: target.clone(),
            used: u32::try_from(repairs.len()).unwrap_or(u32::MAX),
            limit: cfg.max_repairs_per_target_10min,
        })
        .collect();
    targets.sort_by(|left, right| left.target.cmp(&right.target));
    let target_exhausted = targets
        .iter()
        .any(|target| target.used >= cfg.max_repairs_per_target_10min);
    BudgetState {
        root_used,
        root_limit: cfg.max_repairs_per_10min,
        targets,
        exhausted: state.escalated || root_used >= cfg.max_repairs_per_10min || target_exhausted,
    }
}
