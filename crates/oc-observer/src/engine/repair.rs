//! Repair reservation (budget) plus validation and execution of one repair.

use std::time::Duration;

use crate::input::TreeRow;
use crate::registry::StripState;
use crate::verdict::{ObserverVerdict, RepairKind};

use super::ObserverEngine;
use super::budget::budget_from;
use super::notice::{ObserverNotice, ObserverRun, RepairOutcome};
use super::state::prune_all;

enum Reserve {
    Ok {
        used: u32,
        left: u32,
    },
    TargetExhausted,
    Exhausted {
        reason: String,
        budget: super::BudgetState,
    },
}

/// What the host said about one repair.
///
/// A repair is only [`Receipt::Applied`] when the host confirmed it performed
/// the command. Absence of an answer is [`Receipt::Unconfirmed`] — never
/// success, because a success reported without a receipt is exactly the
/// defect this type exists to remove.
enum Receipt {
    /// The host performed the repair.
    Applied(RepairOutcome),
    /// The host answered and refused (catalog-unknown model, nothing to
    /// cancel, …).
    Refused(String),
    /// The host gave no usable answer: executor error, dropped reply channel
    /// or a receipt that never arrived inside `repair_ack_timeout_ms`.
    Unconfirmed(String),
}

impl ObserverEngine {
    /// Ask the host to apply the repair and wait for its receipt.
    ///
    /// The wait is bounded by [`crate::ObserverConfig::repair_ack_timeout_ms`]
    /// so a host that never answers cannot hang the observer tick, and it can
    /// never be mistaken for success.
    async fn repair_receipt(&self, verdict: &ObserverVerdict) -> Receipt {
        let answer = self.executor.apply(verdict);
        let timeout_ms = self.cfg.repair_ack_timeout_ms;
        let answer = if timeout_ms > 0 {
            let budget = Duration::from_millis(u64::try_from(timeout_ms).unwrap_or(u64::MAX));
            match tokio::time::timeout(budget, answer).await {
                Ok(answer) => answer,
                Err(_elapsed) => {
                    return Receipt::Unconfirmed(format!(
                        "repair receipt timed out after {timeout_ms} ms; \
                         the host never confirmed the repair"
                    ));
                }
            }
        } else {
            answer.await
        };
        match answer {
            Ok(outcome) if outcome.applied => Receipt::Applied(outcome),
            Ok(outcome) => Receipt::Refused(outcome.detail.unwrap_or_else(|| {
                "the host did not apply the repair and gave no reason".to_owned()
            })),
            Err(error) => Receipt::Unconfirmed(error.to_string()),
        }
    }

    /// Return a reservation for a repair that was never performed.
    ///
    /// An unsupported or unanswered executor must not drain the budget and
    /// escalate the root on its own, and a repair that did not happen must not
    /// leave a compaction cooldown behind.
    fn release_reservation(&self, root_id: &str, target: &str, kind: RepairKind, now: i64) {
        self.with_root(root_id, |state| {
            if let Some(position) = state.repairs.iter().rposition(|at| *at == now) {
                state.repairs.remove(position);
            }
            if let Some(repairs) = state.target_repairs.get_mut(target)
                && let Some(position) = repairs.iter().rposition(|at| *at == now)
            {
                repairs.remove(position);
            }
            if kind == RepairKind::CompactContext {
                state.compacts.remove(target);
            }
        });
    }

    /// Drop the compaction cooldown for a compaction the host refused.
    ///
    /// The attempt stays charged (a repair the host will never accept must run
    /// out of budget instead of being re-proposed on every tick), but nothing
    /// was compacted, so the next real compaction must not be blocked.
    fn clear_compact_cooldown(&self, root_id: &str, target: &str, kind: RepairKind) {
        if kind == RepairKind::CompactContext {
            self.with_root(root_id, |state| {
                state.compacts.remove(target);
            });
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn apply_repair(
        &self,
        root_id: &str,
        tree: &[TreeRow],
        verdict: &ObserverVerdict,
        now: i64,
    ) -> ObserverRun {
        let target = verdict.target.clone().unwrap_or_default();
        let kind = match self.validate_repair(root_id, tree, verdict, &target, now) {
            Ok(kind) => kind,
            Err(reason) => {
                return ObserverRun::Rejected {
                    action: verdict.action,
                    reason,
                };
            }
        };

        let reserve = self.with_root(root_id, |state| {
            prune_all(state, now, &self.cfg);
            let root_used = u32::try_from(state.repairs.len()).unwrap_or(u32::MAX);
            let target_used = u32::try_from(
                state
                    .target_repairs
                    .get(&target)
                    .map_or(0, std::vec::Vec::len),
            )
            .unwrap_or(u32::MAX);
            let root_exhausted = state.escalated || root_used >= self.cfg.max_repairs_per_10min;
            let target_exhausted = target_used >= self.cfg.max_repairs_per_target_10min;
            if root_exhausted || target_exhausted {
                // Only the root budget escalates the whole tree; one exhausted
                // target is a rejected repair and must not stop supervision of
                // every other session under the root.
                if !root_exhausted {
                    return Reserve::TargetExhausted;
                }
                state.escalated = true;
                return Reserve::Exhausted {
                    reason: "repair budget for this root is exhausted".to_owned(),
                    budget: budget_from(state, &self.cfg),
                };
            }
            state.repairs.push(now);
            state
                .target_repairs
                .entry(target.clone())
                .or_default()
                .push(now);
            if kind == RepairKind::CompactContext {
                state.compacts.insert(target.clone(), now);
            }
            Reserve::Ok {
                used: root_used.saturating_add(1),
                left: self
                    .cfg
                    .max_repairs_per_10min
                    .saturating_sub(root_used.saturating_add(1)),
            }
        });

        let (used, left) = match reserve {
            Reserve::Ok { used, left } => (used, left),
            Reserve::TargetExhausted => {
                return ObserverRun::Rejected {
                    action: verdict.action,
                    reason: "repair budget for this target is exhausted".to_owned(),
                };
            }
            Reserve::Exhausted { reason, budget } => {
                self.notifier.notify(ObserverNotice::Escalated {
                    root_id: root_id.to_owned(),
                    reason: reason.clone(),
                    budget: budget.clone(),
                });
                return ObserverRun::Escalated { reason, budget };
            }
        };

        match self.repair_receipt(verdict).await {
            // The host confirmed it performed the repair: only now may the
            // audit row and `observer.repaired` be published, because the host
            // notifier writes them from this notice.
            Receipt::Applied(outcome) => {
                self.notifier.notify(ObserverNotice::Repaired {
                    root_id: root_id.to_owned(),
                    target,
                    kind,
                    reason: verdict.reason.clone(),
                    attempt: used,
                    budget_left: left,
                });
                ObserverRun::Repair {
                    verdict: verdict.clone(),
                    outcome,
                }
            }
            // The host answered and refused. The attempt is still charged: a
            // permanently impossible repair (for example a model the catalog
            // cannot run) must run out of budget and escalate instead of being
            // proposed on every tick forever. Nothing is published as repaired.
            Receipt::Refused(reason) => {
                // Nothing was compacted, so do not hold the cooldown.
                self.clear_compact_cooldown(root_id, &target, kind);
                self.notifier.notify(ObserverNotice::RepairFailed {
                    root_id: root_id.to_owned(),
                    target,
                    kind,
                    reason: reason.clone(),
                });
                ObserverRun::Rejected {
                    action: verdict.action,
                    reason,
                }
            }
            // No usable answer. Nothing was repaired and the reserve is
            // returned, so an unavailable host cannot escalate the root on its
            // own. This is reported as a failure, never as a repair.
            Receipt::Unconfirmed(reason) => {
                self.release_reservation(root_id, &target, kind, now);
                self.notifier.notify(ObserverNotice::RepairFailed {
                    root_id: root_id.to_owned(),
                    target,
                    kind,
                    reason: reason.clone(),
                });
                ObserverRun::Unavailable { reason }
            }
        }
    }

    fn validate_repair(
        &self,
        root_id: &str,
        tree: &[TreeRow],
        verdict: &ObserverVerdict,
        target: &str,
        now: i64,
    ) -> Result<RepairKind, String> {
        let kind = verdict
            .repair
            .as_ref()
            .and_then(|repair| repair.kind)
            .ok_or_else(|| "repair object has no kind".to_owned())?;
        if target.is_empty() {
            return Err("repair requires a target".to_owned());
        }
        if matches!(kind, RepairKind::RestartSubagent | RepairKind::Repost)
            && self.is_stopped_target(root_id, target, tree)
        {
            return Err("session stopped by the user".to_owned());
        }
        match kind {
            RepairKind::RetryTool => {
                let strip =
                    self.registry
                        .strip(target)
                        .or_else(|| {
                            self.registry.recent(root_id, 32).into_iter().find(|strip| {
                                strip.session_id == target && strip.state.is_failure()
                            })
                        })
                        .ok_or_else(|| format!("unknown tool call target: {target}"))?;
                if !strip.state.is_failure() {
                    return Err("retry_tool requires a failed tool call".to_owned());
                }
                if !strip.retryable {
                    return Err("retry_tool refused: the tool call is not idempotent".to_owned());
                }
            }
            RepairKind::CancelTool => {
                let strip = self
                    .registry
                    .strip(target)
                    .or_else(|| {
                        self.registry
                            .active(root_id)
                            .into_iter()
                            .find(|strip| strip.session_id == target)
                    })
                    .ok_or_else(|| format!("unknown tool call target: {target}"))?;
                if strip.state != StripState::Running {
                    return Err("cancel_tool requires a running tool call".to_owned());
                }
                if strip.tool == "subagent" {
                    return Err(
                        "cancel_tool is not allowed on subagent; restart instead".to_owned()
                    );
                }
            }
            RepairKind::RestartSubagent | RepairKind::Repost => {}
            RepairKind::CompactContext => {
                let last = self.with_root(root_id, |state| state.compacts.get(target).copied());
                if last.is_some_and(|last| now.saturating_sub(last) < self.cfg.compact_cooldown_ms)
                {
                    return Err("compact_context is cooling down for this target".to_owned());
                }
            }
            RepairKind::SwitchModel => {
                if self.cfg.escalation_model.is_none() {
                    return Err("switch_model requires observer.escalation_model".to_owned());
                }
            }
            RepairKind::RequestBudgetExtension => {
                // A budget request only makes sense for a session that is
                // currently executing work under this root. The repair
                // reservation above is the only rate limit: a grant is a
                // turn-loop decision, not a second independent budget.
                if !tree.iter().any(|row| {
                    row.sid == target && (row.state == "busy" || row.state == "waiting_permission")
                }) {
                    return Err(format!(
                        "budget extension requires a busy session: {target}"
                    ));
                }
            }
        }
        Ok(kind)
    }
}
