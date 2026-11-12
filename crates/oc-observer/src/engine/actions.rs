//! Verdict dispatch (`apply`), the nudge/inspect budgets and the goal actions.

use crate::goal::GoalSnapshot;
use crate::input::TreeRow;
use crate::verdict::{ObserverAction, ObserverVerdict};

use super::ObserverEngine;
use super::notice::{ObserverNotice, ObserverRun};
use super::state::prune_vec;

impl ObserverEngine {
    pub(super) async fn apply(
        &self,
        root_id: &str,
        tree: &[TreeRow],
        goal: Option<&GoalSnapshot>,
        verdict: &ObserverVerdict,
        now: i64,
    ) -> ObserverRun {
        match verdict.action {
            ObserverAction::None => ObserverRun::Verdict {
                verdict: verdict.clone(),
            },
            ObserverAction::Nudge => {
                let target = verdict.target.clone().unwrap_or_default();
                if self.is_stopped_target(root_id, &target, tree) {
                    return ObserverRun::Rejected {
                        action: verdict.action,
                        reason: "session stopped by the user".to_owned(),
                    };
                }
                let allowed = self.with_root(root_id, |state| {
                    let window = state.nudges.entry(target.clone()).or_default();
                    prune_vec(window, now, self.cfg.budget_window_ms);
                    if u32::try_from(window.len()).unwrap_or(u32::MAX)
                        >= self.cfg.max_nudges_per_target_10min
                    {
                        return false;
                    }
                    window.push(now);
                    true
                });
                if !allowed {
                    return ObserverRun::Rejected {
                        action: verdict.action,
                        reason: "nudge budget for this target is exhausted".to_owned(),
                    };
                }
                self.notifier.notify(ObserverNotice::Nudged {
                    root_id: root_id.to_owned(),
                    target,
                    message: verdict.message.clone().unwrap_or_default(),
                });
                ObserverRun::Verdict {
                    verdict: verdict.clone(),
                }
            }
            ObserverAction::Inspect => {
                let target = verdict.target.clone().unwrap_or_default();
                let allowed = self.with_root(root_id, |state| {
                    prune_vec(&mut state.inspects, now, self.cfg.budget_window_ms);
                    if u32::try_from(state.inspects.len()).unwrap_or(u32::MAX)
                        >= self.cfg.max_inspects_per_10min
                    {
                        return false;
                    }
                    state.inspects.push(now);
                    true
                });
                if !allowed {
                    return ObserverRun::Rejected {
                        action: verdict.action,
                        reason: "inspect budget is exhausted".to_owned(),
                    };
                }
                self.notifier.notify(ObserverNotice::Inspected {
                    root_id: root_id.to_owned(),
                    target,
                    reason: verdict.reason.clone(),
                });
                ObserverRun::Verdict {
                    verdict: verdict.clone(),
                }
            }
            ObserverAction::Repair => self.apply_repair(root_id, tree, verdict, now).await,
            ObserverAction::GoalDone | ObserverAction::GoalBlocked | ObserverAction::GoalFailed => {
                self.apply_goal(root_id, tree, goal, verdict, now)
            }
        }
    }
}
