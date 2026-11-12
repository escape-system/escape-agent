//! Goal judging: the production goal decision and the evidence candidates the
//! host feeds it.

use crate::goal::{
    self, GoalSnapshot, GoalStatus, GoalTransition, GoalVerdict, GoalVerdictKind, RecentOutcome,
};
use crate::input::TreeRow;
use crate::verdict::{ObserverAction, ObserverVerdict};

use super::ObserverEngine;
use super::notice::{ObserverNotice, ObserverRun};

impl ObserverEngine {
    /// Judge a goal and report the applied transition.
    ///
    /// This is the production goal decision: `goal_complete` requests it and
    /// the host persists the result from [`ObserverNotice::Goal`]. The judge
    /// document is [`crate::goal::judge_prompt`] and the reply is parsed by
    /// [`crate::goal::parse_goal_verdict`], so a verdict that is not a goal
    /// verdict can never be mistaken for one.
    ///
    /// `known_ids` are ids this goal may additionally cite as evidence (the
    /// host passes recent message ids, because `goal_complete` evidence is
    /// usually a `msg_…`); the registry's recent tool-call ids are always
    /// accepted.
    ///
    /// Terminal goals are not judged again, so one completion cannot flip a
    /// goal twice. A model failure fails open: no verdict, one
    /// `observer.unavailable` notice, goal unchanged.
    pub async fn check_goal(
        &self,
        root_id: &str,
        goal: &GoalSnapshot,
        recent: &[RecentOutcome],
        known_ids: &[String],
    ) -> Option<GoalTransition> {
        if !self.cfg.enabled || goal.status.is_terminal() {
            return None;
        }
        let mut snapshot = goal.clone();
        for id in known_ids {
            if !snapshot.known_ids.contains(id) {
                snapshot.known_ids.push(id.clone());
            }
        }
        for ev in &snapshot.evidence {
            if !snapshot.known_ids.contains(ev) {
                snapshot.known_ids.push(ev.clone());
            }
        }
        for strip in self.registry.recent(root_id, 32) {
            if !snapshot.known_ids.contains(&strip.tool_call_id) {
                snapshot.known_ids.push(strip.tool_call_id);
            }
            if !snapshot.known_ids.contains(&strip.session_id) {
                snapshot.known_ids.push(strip.session_id);
            }
        }
        if !snapshot.known_ids.iter().any(|id| id == root_id) {
            snapshot.known_ids.push(root_id.to_owned());
        }
        let prompt = goal::judge_prompt(&snapshot, recent);
        let text = match self.model.judge(&prompt, &self.cfg).await {
            Ok(text) => text,
            Err(error) => {
                self.notify_unavailable(root_id, &error.to_string());
                return None;
            }
        };
        let verdict = match goal::parse_goal_verdict(&text) {
            Ok(verdict) => verdict,
            Err(error) => {
                self.notify_unavailable(root_id, &error.to_string());
                return None;
            }
        };
        let transition = goal::apply_verdict(&snapshot, &verdict, self.clock.now_ms());
        self.notifier.notify(ObserverNotice::Goal {
            root_id: root_id.to_owned(),
            goal_id: transition.goal.id.clone(),
            status: transition.goal.status,
            reason: transition.reason.clone(),
            goal: transition.goal.clone(),
        });
        Some(transition)
    }

    /// Settled strips of a root, shaped as goal evidence candidates.
    #[must_use]
    pub fn recent_outcomes(&self, root_id: &str, limit: usize) -> Vec<RecentOutcome> {
        self.registry
            .recent(root_id, limit)
            .into_iter()
            .map(|strip| RecentOutcome {
                id: strip.tool_call_id,
                tool: strip.tool,
                outcome: strip.outcome,
            })
            .collect()
    }

    pub(super) fn apply_goal(
        &self,
        root_id: &str,
        tree: &[TreeRow],
        goal: Option<&GoalSnapshot>,
        verdict: &ObserverVerdict,
        now: i64,
    ) -> ObserverRun {
        let Some(goal) = goal else {
            return ObserverRun::Rejected {
                action: verdict.action,
                reason: "no active goal to update".to_owned(),
            };
        };
        if goal.status.is_terminal() {
            return ObserverRun::Verdict {
                verdict: verdict.clone(),
            };
        }
        let mut snapshot = goal.clone();
        let mut known = snapshot.known_ids.clone();
        for ev in &snapshot.evidence {
            if !known.contains(ev) {
                known.push(ev.clone());
            }
        }
        for strip in self.registry.recent(root_id, 32) {
            if !known.contains(&strip.tool_call_id) {
                known.push(strip.tool_call_id);
            }
            if !known.contains(&strip.session_id) {
                known.push(strip.session_id);
            }
        }
        for row in tree {
            if !known.contains(&row.sid) {
                known.push(row.sid.clone());
            }
        }
        snapshot.known_ids = known;

        let transition: GoalTransition = match verdict.action {
            ObserverAction::GoalDone => goal::apply_verdict(
                &snapshot,
                &GoalVerdict {
                    verdict: Some(GoalVerdictKind::Done),
                    reason: verdict.reason.clone(),
                    next_step: None,
                    evidence: verdict.goal_evidence.clone(),
                    wait_minutes: None,
                },
                now,
            ),
            ObserverAction::GoalBlocked => goal::apply_verdict(
                &snapshot,
                &GoalVerdict {
                    verdict: Some(GoalVerdictKind::Blocked),
                    reason: verdict.reason.clone(),
                    next_step: None,
                    evidence: verdict.goal_evidence.clone(),
                    wait_minutes: None,
                },
                now,
            ),
            _ => {
                let mut failed = snapshot;
                failed.status = GoalStatus::Failed;
                failed.rounds = failed.rounds.saturating_add(1);
                failed.wait_until_ms = None;
                failed.wait_started_ms = None;
                GoalTransition {
                    applied: true,
                    goal: failed,
                    reason: verdict.reason.clone(),
                }
            }
        };
        self.notifier.notify(ObserverNotice::Goal {
            root_id: root_id.to_owned(),
            goal_id: transition.goal.id.clone(),
            status: transition.goal.status,
            reason: transition.reason.clone(),
            goal: transition.goal.clone(),
        });
        ObserverRun::Verdict {
            verdict: verdict.clone(),
        }
    }
}
