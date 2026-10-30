//! The observer loop: wake decisions, coalescing, repair budgets and actions.
//!
//! Everything the loop needs from the outside world is a trait: time
//! ([`Clock`]), the model ([`ObserverModel`]), repair execution
//! ([`RepairExecutor`]) and notice delivery ([`ObserverNotifier`]). Persistence
//! and HTTP live in the host; this module only decides.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::clock::Clock;
use crate::config::ObserverConfig;
use crate::error::ObserverError;
use crate::goal::{self, GoalSnapshot, GoalStatus, GoalTransition, GoalVerdict, GoalVerdictKind};
use crate::input::{self, ActiveStrip, ObserverInput, RecentStrip, RootInput, TreeRow};
use crate::registry::{ExpectationRegistry, FlightStrip, RegistryCaps, StripState};
use crate::verdict::{self, ObserverAction, ObserverVerdict, RepairKind};

/// A boxed, sendable future used by the host-supplied traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The observer's model call.
pub trait ObserverModel: Send + Sync {
    /// Judge one rendered input document. The returned text is strict JSON
    /// parsed by [`crate::parse_verdict`].
    fn judge<'a>(
        &'a self,
        input: &'a str,
        cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>>;
}

/// Host-side execution of a validated repair command.
pub trait RepairExecutor: Send + Sync {
    /// Apply the repair (re-run a tool, restart a child, compact, …).
    fn apply<'a>(
        &'a self,
        verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>>;
}

/// Notice sink. The server maps these to `observer.*` events and synthetic
/// messages; a test records them in a vector.
pub trait ObserverNotifier: Send + Sync {
    /// Publish one notice.
    fn notify(&self, event: ObserverNotice);
}

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
    /// A step completed (activity, not a wake on its own).
    StepEvent,
}

#[derive(Debug, Default)]
struct RootState {
    root_context: crate::input::RootContext,
    last_call_ms: Option<i64>,
    calls: Vec<i64>,
    last_activity_ms: Option<i64>,
    error_streak: u32,
    pending_wake: bool,
    last_goal: Option<(GoalStatus, u32)>,
    escalated: bool,
    last_unavailable_notice_ms: Option<i64>,
    repairs: Vec<i64>,
    target_repairs: HashMap<String, Vec<i64>>,
    nudges: HashMap<String, Vec<i64>>,
    inspects: Vec<i64>,
    compacts: HashMap<String, i64>,
}

/// Per-root observer engine.
pub struct ObserverEngine {
    cfg: ObserverConfig,
    clock: Arc<dyn Clock>,
    model: Arc<dyn ObserverModel>,
    executor: Arc<dyn RepairExecutor>,
    notifier: Arc<dyn ObserverNotifier>,
    registry: ExpectationRegistry,
    state: Mutex<HashMap<String, RootState>>,
}

impl fmt::Debug for ObserverEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObserverEngine")
            .field("cfg", &self.cfg)
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

enum Gate {
    Proceed,
    Skip(SkipReason),
    Escalated { reason: String, budget: BudgetState },
}

enum Reserve {
    Ok { used: u32, left: u32 },
    Exhausted { reason: String, budget: BudgetState },
}

impl ObserverEngine {
    /// Build an engine. The registry shares the supplied clock.
    #[must_use]
    pub fn new(
        cfg: ObserverConfig,
        clock: Arc<dyn Clock>,
        model: Arc<dyn ObserverModel>,
        executor: Arc<dyn RepairExecutor>,
        notifier: Arc<dyn ObserverNotifier>,
    ) -> Self {
        Self {
            registry: ExpectationRegistry::new(clock.clone()),
            cfg,
            clock,
            model,
            executor,
            notifier,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Override registry retention caps (must be called before feeding strips).
    #[must_use]
    pub fn with_registry_caps(mut self, caps: RegistryCaps) -> Self {
        self.registry = ExpectationRegistry::with_caps(self.clock.clone(), caps);
        self
    }

    /// The underlying strip registry.
    #[must_use]
    pub fn registry(&self) -> &ExpectationRegistry {
        &self.registry
    }

    /// Configuration in use.
    #[must_use]
    pub fn config(&self) -> &ObserverConfig {
        &self.cfg
    }

    /// Register a tool-call strip.
    pub fn observe_start(&self, strip: FlightStrip) {
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
    pub fn note_host_event(&self, root_id: &str, flag: HostFlag) {
        let now = self.clock.now_ms();
        self.with_root(root_id, |root| match flag {
            HostFlag::StepEvent => root.last_activity_ms = Some(now),
            HostFlag::ExecutionFailed | HostFlag::ExecutionInterrupted => {
                root.pending_wake = true;
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
    pub fn budget(&self, root_id: &str) -> BudgetState {
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
        goal: Option<GoalSnapshot>,
    ) -> Option<ObserverRun> {
        self.run(root_id, tree, goal, false).await
    }

    /// Forced check (UI `POST /observer/check`). Bypasses wake conditions but
    /// still respects coalescing and budgets.
    pub async fn check_now(
        &self,
        root_id: &str,
        tree: Vec<TreeRow>,
        goal: Option<GoalSnapshot>,
    ) -> Option<ObserverRun> {
        self.run(root_id, tree, goal, true).await
    }

    async fn run(
        &self,
        root_id: &str,
        tree: Vec<TreeRow>,
        goal: Option<GoalSnapshot>,
        force: bool,
    ) -> Option<ObserverRun> {
        if !self.cfg.enabled {
            return None;
        }
        let now = self.clock.now_ms();
        let goal_changed = self.with_root(root_id, |state| {
            let current = goal.as_ref().map(|goal| (goal.status, goal.rounds));
            let changed = current != state.last_goal && current.is_some();
            state.last_goal = current;
            changed
        });
        if goal_changed {
            self.with_root(root_id, |state| state.pending_wake = true);
        }

        let active_strips = self.registry.active(root_id);
        let busy = tree.iter().any(|row| row.state == "busy");
        let stall = busy
            && active_strips.is_empty()
            && self.with_root(root_id, |state| {
                state
                    .last_activity_ms
                    .is_some_and(|last| now.saturating_sub(last) >= self.cfg.stall_seconds * 1_000)
            });
        let overdue = self
            .registry
            .earliest_deadline(root_id)
            .is_some_and(|deadline| deadline <= now);
        let pending = self.with_root(root_id, |state| {
            state.pending_wake || state.error_streak >= 3
        });

        if !force && !overdue && !pending && !goal_changed && !stall {
            return None;
        }

        let gate = self.with_root(root_id, |state| {
            prune_all(state, now, &self.cfg);
            if state.escalated {
                return Gate::Escalated {
                    reason: "repair budget exhausted".to_owned(),
                    budget: budget_from(state, &self.cfg),
                };
            }
            let calls = state.calls.len();
            let rate_limited = u32::try_from(calls).unwrap_or(u32::MAX)
                >= self.cfg.max_calls_per_min
                || state
                    .last_call_ms
                    .is_some_and(|last| now.saturating_sub(last) < self.cfg.min_interval_ms);
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
            state.last_call_ms = Some(now);
            state.calls.push(now);
            state.last_activity_ms = Some(now);
            Gate::Proceed
        });

        match gate {
            Gate::Skip(reason) => return Some(ObserverRun::Skipped { reason }),
            Gate::Escalated { reason, budget } => {
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
        Some(self.apply(root_id, goal.as_ref(), &verdict, now).await)
    }

    fn build_input(
        &self,
        root_id: &str,
        tree: &[TreeRow],
        goal: Option<&GoalSnapshot>,
        now: i64,
    ) -> ObserverInput {
        let mut active: Vec<ActiveStrip> = self
            .registry
            .active(root_id)
            .into_iter()
            .map(|strip| {
                let age_s = strip.elapsed_s(now);
                let overdue = strip.is_overdue(now);
                ActiveStrip {
                    sid: strip.session_id,
                    agent: strip.agent,
                    tool: strip.tool,
                    why: strip.why,
                    age_s,
                    expect_s: strip.expect_s,
                    intervene_s: strip.intervene_s,
                    state: strip.state,
                    overdue,
                }
            })
            .collect();
        active.sort_by_key(|strip| {
            (
                !strip.overdue,
                i64::from(strip.intervene_s).saturating_sub(strip.age_s),
            )
        });

        let recent: Vec<RecentStrip> = self
            .registry
            .recent(root_id, 12)
            .into_iter()
            .map(|strip| {
                let took_s = strip.elapsed_s(now);
                RecentStrip {
                    sid: strip.session_id,
                    tool: strip.tool,
                    outcome: strip.outcome,
                    took_s,
                    why: strip.why,
                }
            })
            .collect();

        let root_row = tree.iter().find(|row| row.sid == root_id);
        let context = self.with_root(root_id, |state| state.root_context.clone());
        let root = RootInput {
            id: root_id.to_owned(),
            agent: context
                .agent
                .or_else(|| root_row.map(|row| row.agent.clone()))
                .unwrap_or_else(|| "build".to_owned()),
            model: context.model,
            step: context.step,
            state: context
                .state
                .or_else(|| root_row.map(|row| row.state.clone()))
                .unwrap_or_else(|| {
                    if active.is_empty() {
                        "idle".to_owned()
                    } else {
                        "busy".to_owned()
                    }
                }),
        };

        ObserverInput {
            goal: goal.map(GoalSnapshot::to_input),
            root,
            active,
            recent,
            tree: tree.to_vec(),
        }
    }

    async fn apply(
        &self,
        root_id: &str,
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
            ObserverAction::Repair => self.apply_repair(root_id, verdict, now).await,
            ObserverAction::GoalDone | ObserverAction::GoalBlocked | ObserverAction::GoalFailed => {
                self.apply_goal(root_id, goal, verdict, now)
            }
        }
    }

    async fn apply_repair(
        &self,
        root_id: &str,
        verdict: &ObserverVerdict,
        now: i64,
    ) -> ObserverRun {
        let target = verdict.target.clone().unwrap_or_default();
        let kind = match self.validate_repair(root_id, verdict, &target, now) {
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
            if state.escalated
                || root_used >= self.cfg.max_repairs_per_10min
                || target_used >= self.cfg.max_repairs_per_target_10min
            {
                state.escalated = true;
                let reason = if state.escalated && root_used >= self.cfg.max_repairs_per_10min {
                    "repair budget for this root is exhausted".to_owned()
                } else if target_used >= self.cfg.max_repairs_per_target_10min {
                    "repair budget for this target is exhausted".to_owned()
                } else {
                    "repair budget exhausted".to_owned()
                };
                return Reserve::Exhausted {
                    reason,
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
            Reserve::Exhausted { reason, budget } => {
                self.notifier.notify(ObserverNotice::Escalated {
                    root_id: root_id.to_owned(),
                    reason: reason.clone(),
                    budget: budget.clone(),
                });
                return ObserverRun::Escalated { reason, budget };
            }
        };

        match self.executor.apply(verdict).await {
            Ok(outcome) => {
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
            Err(error) => {
                let reason = error.to_string();
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
        match kind {
            RepairKind::RetryTool => {
                let strip = self
                    .registry
                    .strip(target)
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
        }
        Ok(kind)
    }

    fn apply_goal(
        &self,
        root_id: &str,
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
        let mut snapshot = goal.clone();
        let mut known = snapshot.known_ids.clone();
        for strip in self.registry.recent(root_id, 32) {
            if !known.contains(&strip.tool_call_id) {
                known.push(strip.tool_call_id);
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

    fn notify_unavailable(&self, root_id: &str, reason: &str) {
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
            self.notifier.notify(ObserverNotice::Unavailable {
                root_id: root_id.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }

    fn with_root<T>(&self, root_id: &str, f: impl FnOnce(&mut RootState) -> T) -> T {
        let mut map = self.state.lock();
        f(map.entry(root_id.to_owned()).or_default())
    }
}

fn prune_all(state: &mut RootState, now: i64, cfg: &ObserverConfig) {
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
}

fn prune_vec(values: &mut Vec<i64>, now: i64, window_ms: i64) {
    values.retain(|value| now.saturating_sub(*value) < window_ms);
}

fn budget_from(state: &RootState, cfg: &ObserverConfig) -> BudgetState {
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
