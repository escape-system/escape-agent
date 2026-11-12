//! The observer loop: the coalescing/rate-limit/escalation gate, the user-stop
//! guards, wake decisions and the model call. Timer ticks and forced checks
//! differ only by `force`.

use crate::clock::EngineNow;
use crate::input::{self, TreeRow};
use crate::registry::{MS_PER_SECOND, StripState};
use crate::verdict;

use super::ObserverEngine;
use super::budget::budget_from;
use super::notice::{ObserverRun, SkipReason};
use super::state::{MIN_OVERDUE_BACKOFF_MS, prune_all};
use super::wake::{is_strip_overdue, is_user_stopped_row};

enum Gate {
    Proceed,
    Skip(SkipReason),
    Escalated {
        reason: String,
        budget: super::BudgetState,
    },
}

impl ObserverEngine {
    /// Register a tool-call strip.
    pub fn observe_start(&self, strip: crate::registry::FlightStrip) {
        let root_id = strip.root_id.clone();
        let now = self.clock.now_ms();
        self.registry.open(strip);
        self.with_root(&root_id, |state| {
            state.last_activity_ms = Some(now);
        });
    }

    /// Settle a strip and update wake bookkeeping.
    pub fn observe_settle(&self, tool_call_id: &str, state: StripState, outcome: Option<&str>) {
        let previous = self.registry.strip(tool_call_id);
        if !self.registry.settle(tool_call_id, state, outcome) {
            return;
        }
        let Some(previous) = previous else { return };
        if previous.state != StripState::Running {
            return;
        }
        let now = self.clock.now_ms();
        let root_id = previous.root_id;
        self.with_root(&root_id, |root| {
            root.last_activity_ms = Some(now);
            if state.is_failure() {
                root.pending_wake = true;
            }
            if state == StripState::Error {
                root.error_streak = root.error_streak.saturating_add(1);
                if root.error_streak >= 3 {
                    root.pending_wake = true;
                }
            } else {
                root.error_streak = 0;
            }
        });
    }

    /// Set the root model/step/agent header fed to the next model call.
    pub fn set_root_context(&self, root_id: &str, context: crate::input::RootContext) {
        self.with_root(root_id, |state| state.root_context = context);
    }

    /// Record a host execution signal.
    pub fn note_host_event(&self, root_id: &str, flag: super::HostFlag) {
        use super::HostFlag;
        let now = self.clock.now_ms();
        self.with_root(root_id, |root| match flag {
            HostFlag::StepEvent => root.last_activity_ms = Some(now),
            HostFlag::ExecutionFailed | HostFlag::ExecutionInterrupted => {
                root.pending_wake = true;
                root.last_activity_ms = Some(now);
            }
            HostFlag::ExecutionInterruptedByUser => {
                // Terminal for the observer: no wake, no re-raise.
                root.pending_wake = false;
                root.user_stopped = true;
                root.last_activity_ms = Some(now);
            }
            HostFlag::ExecutionStarted => {
                root.user_stopped = false;
                root.last_activity_ms = Some(now);
            }
        });
    }

    /// Earliest scheduled observer deadline for a root, if any strip runs.
    #[must_use]
    pub fn earliest_deadline(&self, root_id: &str) -> Option<i64> {
        self.registry.earliest_deadline(root_id)
    }

    /// Current repair budget for a root.
    #[must_use]
    pub fn budget(&self, root_id: &str) -> super::BudgetState {
        let now = self.clock.now_ms();
        self.with_root(root_id, |state| {
            prune_all(state, now, &self.cfg);
            budget_from(state, &self.cfg)
        })
    }

    /// Timer-driven tick. Calls the model only when a wake condition holds.
    pub async fn tick(
        &self,
        root_id: &str,
        tree: Vec<TreeRow>,
        goal: Option<crate::goal::GoalSnapshot>,
    ) -> Option<ObserverRun> {
        self.run(root_id, tree, goal, false).await
    }

    /// Forced check (UI `POST /observer/check`). Bypasses wake conditions but
    /// still respects coalescing and budgets.
    pub async fn check_now(
        &self,
        root_id: &str,
        tree: Vec<TreeRow>,
        goal: Option<crate::goal::GoalSnapshot>,
    ) -> Option<ObserverRun> {
        self.run(root_id, tree, goal, true).await
    }

    #[allow(clippy::too_many_lines)]
    async fn run(
        &self,
        root_id: &str,
        tree: Vec<TreeRow>,
        goal: Option<crate::goal::GoalSnapshot>,
        force: bool,
    ) -> Option<ObserverRun> {
        if !self.cfg.enabled {
            return None;
        }
        let now = EngineNow::from_clock(self.clock.as_ref());
        let goal_changed = self.with_root(root_id, |state| {
            let current = goal.as_ref().map(|goal| (goal.status, goal.rounds));
            let changed = current != state.last_goal
                && goal.as_ref().is_some_and(|goal| !goal.status.is_terminal());
            state.last_goal = current;
            changed
        });
        if goal_changed {
            self.with_root(root_id, |state| state.pending_wake = true);
        }

        // A user stop is terminal until the root actually runs again. The
        // durable signal lives on the tree rows (`interrupted` + `user`); the
        // host flag covers the window before that row is read back.
        let root_active = tree
            .iter()
            .any(|row| row.sid == root_id && row.state == "busy");
        if root_active {
            self.with_root(root_id, |state| state.user_stopped = false);
        }
        let user_stopped = self.user_stopped(root_id, &tree);
        if user_stopped {
            // Terminal for both timer ticks and manual checks: a forced check
            // must never re-raise a session the user stopped.
            return None;
        }

        let active_strips = self.registry.active(root_id);
        let busy = tree.iter().any(|row| row.state == "busy");
        let stall = busy
            && active_strips.is_empty()
            && self.with_root(root_id, |state| {
                state.last_activity_ms.is_some_and(|last| {
                    now.wall_ms.saturating_sub(last) >= self.cfg.stall_seconds * MS_PER_SECOND
                })
            });
        let overdue_ids: Vec<(String, u32)> = self.with_root(root_id, |state| {
            state
                .snoozed
                .retain(|id, _| active_strips.iter().any(|strip| &strip.tool_call_id == id));
            active_strips
                .iter()
                .filter(|strip| is_strip_overdue(strip, &tree, now))
                .filter(|strip| {
                    state
                        .snoozed
                        .get(&strip.tool_call_id)
                        .is_none_or(|until| *until <= now.mono_ms)
                })
                .map(|strip| (strip.tool_call_id.clone(), strip.intervene_s))
                .collect()
        });
        let overdue = !overdue_ids.is_empty();
        let pending = self.with_root(root_id, |state| {
            state.pending_wake || state.error_streak >= 3
        });

        if !force
            && !overdue
            && !stall
            && !busy
            && active_strips.is_empty()
            && goal.as_ref().is_some_and(|goal| goal.status.is_terminal())
        {
            self.with_root(root_id, |state| {
                state.pending_wake = false;
                state.error_streak = 0;
            });
            return None;
        }

        if !force && !overdue && !pending && !goal_changed && !stall {
            return None;
        }

        let gate = self.with_root(root_id, |state| {
            prune_all(state, now.wall_ms, &self.cfg);
            if state.escalated {
                let root_used = u32::try_from(state.repairs.len()).unwrap_or(u32::MAX);
                if root_used < self.cfg.max_repairs_per_10min {
                    // The repair window rolled over: escalation is a budget
                    // state, not a permanent latch.
                    state.escalated = false;
                } else {
                    // Consume the wake so an escalated root does not re-enter
                    // this branch on every tick.
                    state.pending_wake = false;
                    state.error_streak = 0;
                    return Gate::Escalated {
                        reason: "repair budget exhausted".to_owned(),
                        budget: budget_from(state, &self.cfg),
                    };
                }
            }
            let calls = state.calls.len();
            let rate_limited = u32::try_from(calls).unwrap_or(u32::MAX)
                >= self.cfg.max_calls_per_min
                || state.last_call_ms.is_some_and(|last| {
                    now.wall_ms.saturating_sub(last) < self.cfg.min_interval_ms
                });
            if rate_limited {
                let reason =
                    if u32::try_from(calls).unwrap_or(u32::MAX) >= self.cfg.max_calls_per_min {
                        SkipReason::RateLimited
                    } else {
                        SkipReason::Coalesced
                    };
                return Gate::Skip(reason);
            }
            state.pending_wake = false;
            state.error_streak = 0;
            state.last_call_ms = Some(now.wall_ms);
            state.calls.push(now.wall_ms);
            state.last_activity_ms = Some(now.wall_ms);
            for (id, intervene_s) in &overdue_ids {
                let backoff = (i64::from(*intervene_s) * MS_PER_SECOND).max(MIN_OVERDUE_BACKOFF_MS);
                state
                    .snoozed
                    .insert(id.clone(), now.mono_ms.saturating_add(backoff));
            }
            Gate::Proceed
        });

        match gate {
            Gate::Skip(reason) => return Some(ObserverRun::Skipped { reason }),
            Gate::Escalated { reason, budget } => {
                // A timer tick against an escalated root is not an evaluation:
                // report it only to explicit checks so hosts do not log one
                // escalation row per tick.
                if !force {
                    return None;
                }
                return Some(ObserverRun::Escalated { reason, budget });
            }
            Gate::Proceed => {}
        }

        let input = self.build_input(root_id, &tree, goal.as_ref(), now);
        let prompt = input::render(&input, self.cfg.max_input_tokens);
        let text = match self.model.judge(&prompt, &self.cfg).await {
            Ok(text) => text,
            Err(error) => {
                let reason = error.to_string();
                self.notify_unavailable(root_id, &reason);
                return Some(ObserverRun::Unavailable { reason });
            }
        };
        let verdict = match verdict::parse_verdict(&text, &self.cfg) {
            Ok(verdict) => verdict,
            Err(error) => {
                let reason = error.to_string();
                self.notify_unavailable(root_id, &reason);
                return Some(ObserverRun::Unavailable { reason });
            }
        };
        Some(
            self.apply(root_id, &tree, goal.as_ref(), &verdict, now.wall_ms)
                .await,
        )
    }

    /// True when the root is stopped by the user.
    ///
    /// Any `interrupted` row with `idle_reason=user` in the tree counts, and
    /// the in-memory host flag is OR-ed in so a stop is honored even before the
    /// durable row is visible to this tick.
    pub(super) fn user_stopped(&self, root_id: &str, tree: &[TreeRow]) -> bool {
        let durable = tree.iter().any(is_user_stopped_row);
        durable || self.with_root(root_id, |state| state.user_stopped)
    }

    /// True when `target` is a user-stopped session.
    pub(super) fn is_stopped_target(&self, root_id: &str, target: &str, tree: &[TreeRow]) -> bool {
        if target.is_empty() {
            return false;
        }
        if let Some(row) = tree.iter().find(|row| row.sid == target) {
            return is_user_stopped_row(row);
        }
        target == root_id && self.with_root(root_id, |state| state.user_stopped)
    }

    pub(super) fn notify_unavailable(&self, root_id: &str, reason: &str) {
        let now = self.clock.now_ms();
        let allowed = self.with_root(root_id, |state| {
            if state.last_unavailable_notice_ms.is_some_and(|last| {
                now.saturating_sub(last) < self.cfg.unavailable_notice_interval_ms
            }) {
                return false;
            }
            state.last_unavailable_notice_ms = Some(now);
            true
        });
        if allowed {
            self.notifier.notify(super::ObserverNotice::Unavailable {
                root_id: root_id.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }

    pub(super) fn with_root<T>(
        &self,
        root_id: &str,
        f: impl FnOnce(&mut super::state::RootState) -> T,
    ) -> T {
        let mut map = self.state.lock();
        f(map.entry(root_id.to_owned()).or_default())
    }
}
