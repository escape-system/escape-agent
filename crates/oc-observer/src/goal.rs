//! The goal state machine and its judging prompt.
//!
//! The observer owns goal state; the host persists [`GoalSnapshot`] and
//! supplies it back on every tick. This module performs no I/O.

use serde::{Deserialize, Serialize};

use crate::input::GoalInput;

/// Goal lifecycle (`docs/08` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Being worked on.
    #[default]
    Active,
    /// Temporarily halted by the observer.
    Paused,
    /// Waiting on a user answer or an external event.
    Blocked,
    /// Completed with evidence.
    Done,
    /// Failed after a limit or an explicit verdict.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

impl GoalStatus {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Terminal statuses accept no further verdicts.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// Per-goal limits (mirrors [`crate::GoalConfig`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalPolicy {
    /// Round ceiling.
    pub max_rounds: u32,
    /// Consecutive no-progress rounds.
    pub max_stalls: u32,
    /// Total wait budget.
    pub wait_limit_minutes: u32,
}

impl Default for GoalPolicy {
    fn default() -> Self {
        Self {
            max_rounds: 50,
            max_stalls: 3,
            wait_limit_minutes: 30,
        }
    }
}

impl From<crate::GoalConfig> for GoalPolicy {
    fn from(config: crate::GoalConfig) -> Self {
        Self {
            max_rounds: config.max_rounds,
            max_stalls: config.max_stalls,
            wait_limit_minutes: config.wait_limit_minutes,
        }
    }
}

/// The observer's view of a goal, plus the ids that evidence may reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GoalSnapshot {
    /// Goal id (`gol_…`).
    pub id: String,
    /// One-line objective.
    pub objective: String,
    /// Criteria lines.
    pub criteria: Vec<String>,
    /// Criteria already satisfied.
    pub done: u32,
    /// Total criteria.
    pub total: u32,
    /// Lifecycle status.
    pub status: GoalStatus,
    /// Rounds judged so far.
    pub rounds: u32,
    /// Consecutive rounds without progress.
    pub stalls: u32,
    /// Evidence ids accumulated so far.
    pub evidence: Vec<String>,
    /// Scheduled re-check for a `wait` verdict.
    pub wait_until_ms: Option<i64>,
    /// First wait of the current no-progress stretch (epoch ms). The total
    /// wait budget is measured from here.
    pub wait_started_ms: Option<i64>,
    /// Last time progress was recorded (epoch ms).
    pub last_progress_ms: i64,
    /// Ids the goal may cite as evidence — recent tool-call / message ids
    /// supplied by the host on this tick. Evidence not in this list is
    /// rejected.
    pub known_ids: Vec<String>,
    /// Limits applied by [`apply_verdict`].
    pub policy: GoalPolicy,
}

impl GoalSnapshot {
    /// Compact header for the observer input document.
    #[must_use]
    pub fn to_input(&self) -> GoalInput {
        GoalInput {
            id: self.id.clone(),
            objective: self.objective.clone(),
            status: self.status.as_str().to_owned(),
            criteria: self.criteria.clone(),
            done: self.done,
            total: self.total,
        }
    }
}

/// Verdicts the goal judge may return (`done | continue | blocked | paused |
/// wait`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalVerdictKind {
    /// All criteria are satisfied by evidence.
    Done,
    /// Keep working.
    Continue,
    /// Needs a user answer or external unblock.
    Blocked,
    /// Paused by policy.
    Paused,
    /// Re-check later.
    Wait,
}

/// A parsed goal verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GoalVerdict {
    /// Verdict kind.
    pub verdict: Option<GoalVerdictKind>,
    /// Why.
    pub reason: String,
    /// What to do next.
    pub next_step: Option<String>,
    /// Evidence ids.
    pub evidence: Vec<String>,
    /// Requested wait, minutes.
    pub wait_minutes: Option<u32>,
}

/// One recent outcome shown to the goal judge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentOutcome {
    /// Tool-call or message id, usable as evidence.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// One-line result.
    pub outcome: Option<String>,
}

/// The result of applying a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalTransition {
    /// Whether the goal was updated.
    pub applied: bool,
    /// Goal after the verdict (unchanged when `applied == false`).
    pub goal: GoalSnapshot,
    /// Human-readable explanation (a rejection reason or the winning verdict).
    pub reason: String,
}

/// Render the strict goal-judging prompt.
#[must_use]
pub fn judge_prompt(goal: &GoalSnapshot, recent_outcomes: &[RecentOutcome]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(512);
    out.push_str(
        "You judge whether a coding goal is complete. Answer with one JSON object only.\n\n",
    );
    let _ = writeln!(
        out,
        "<goal id=\"{}\" status=\"{}\" round=\"{}\" done=\"{}\" total=\"{}\">",
        goal.id,
        goal.status.as_str(),
        goal.rounds,
        goal.done,
        goal.total
    );
    let _ = writeln!(out, "Objective: {}", goal.objective);
    out.push_str("Criteria:\n");
    if goal.criteria.is_empty() {
        out.push_str("1. (none declared)\n");
    } else {
        for (index, criterion) in goal.criteria.iter().enumerate() {
            let marker = if goal.evidence.is_empty() {
                "open"
            } else {
                "see evidence"
            };
            let _ = writeln!(out, "{}. {} [{marker}]", index + 1, criterion);
        }
    }
    if !goal.evidence.is_empty() {
        let _ = writeln!(out, "Evidence so far: {}", goal.evidence.join(", "));
    }
    out.push_str("</goal>\n\nRecent outcomes:\n");
    if recent_outcomes.is_empty() {
        out.push_str("(none)\n");
    } else {
        for outcome in recent_outcomes {
            let result = outcome.outcome.as_deref().unwrap_or("no result");
            let _ = writeln!(out, "- {} [{}]: {}", outcome.id, outcome.tool, result);
        }
    }
    out.push_str(
        "\nReply with JSON: {\"verdict\":\"done|continue|blocked|paused|wait\",\
         \"reason\":\"...\",\"next_step\":\"...\",\"evidence\":[\"tool_call_id\"],\
         \"wait_minutes\":30}\n\
         Rules: done requires evidence ids from the recent list or evidence so far; wait at most the configured \
         limit; use blocked only for a user or external dependency.\n",
    );
    out
}

/// Parse a goal verdict document (bare JSON or one Markdown fence).
pub fn parse_goal_verdict(text: &str) -> Result<GoalVerdict, crate::VerdictError> {
    use crate::verdict::VerdictError;
    use serde_json::Value;

    let body = crate::verdict::unwrap_fence(text);
    let value: Value =
        serde_json::from_str(body).map_err(|error| VerdictError::InvalidJson(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| VerdictError::InvalidJson("expected an object".to_owned()))?;
    let verdict_name = object
        .get("verdict")
        .and_then(Value::as_str)
        .ok_or(VerdictError::MissingField("verdict"))?;
    let verdict = match verdict_name {
        "done" => GoalVerdictKind::Done,
        "continue" => GoalVerdictKind::Continue,
        "blocked" => GoalVerdictKind::Blocked,
        "paused" => GoalVerdictKind::Paused,
        "wait" => GoalVerdictKind::Wait,
        other => {
            return Err(VerdictError::InvalidJson(format!(
                "unknown goal verdict: {other}"
            )));
        }
    };
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let next_step = object
        .get("next_step")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned);
    let evidence = match object.get("evidence") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        Some(_) => {
            return Err(VerdictError::InvalidJson(
                "evidence must be an array".to_owned(),
            ));
        }
    };
    let wait_minutes = object
        .get("wait_minutes")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    Ok(GoalVerdict {
        verdict: Some(verdict),
        reason,
        next_step,
        evidence,
        wait_minutes,
    })
}

/// Apply one verdict to a goal snapshot.
///
/// `known_ids` are the ids the host observed recently; a `done` verdict is
/// only accepted when every evidence id is in that list. Progress resets
/// stalls; consecutive no-progress rounds fail the goal at `max_stalls`, and
/// `max_rounds` fails it at the round ceiling. A wait beyond the total wait
/// budget blocks the goal.
#[must_use]
pub fn apply_verdict(goal: &GoalSnapshot, verdict: &GoalVerdict, now: i64) -> GoalTransition {
    let mut next = goal.clone();
    if goal.status.is_terminal() {
        return GoalTransition {
            applied: false,
            goal: next,
            reason: format!("goal is already {}", goal.status.as_str()),
        };
    }

    let mut applied = true;
    let mut reason = verdict.reason.clone();
    match verdict.verdict.unwrap_or(GoalVerdictKind::Continue) {
        GoalVerdictKind::Done => {
            if !apply_done(&mut next, goal, verdict) {
                applied = false;
                "done verdict rejected: evidence must reference recent tool or message ids"
                    .clone_into(&mut reason);
            }
        }
        GoalVerdictKind::Continue => apply_continue(&mut next, goal, verdict, now),
        GoalVerdictKind::Blocked => {
            next.rounds = next.rounds.saturating_add(1);
            next.status = GoalStatus::Blocked;
            next.wait_until_ms = None;
            next.wait_started_ms = None;
        }
        GoalVerdictKind::Paused => {
            next.rounds = next.rounds.saturating_add(1);
            next.status = GoalStatus::Paused;
            next.wait_until_ms = None;
            next.wait_started_ms = None;
        }
        GoalVerdictKind::Wait => apply_wait(&mut next, verdict, now, &mut reason),
    }

    if !matches!(
        next.status,
        GoalStatus::Done
            | GoalStatus::Failed
            | GoalStatus::Cancelled
            | GoalStatus::Blocked
            | GoalStatus::Paused
    ) {
        if next.rounds >= next.policy.max_rounds {
            next.status = GoalStatus::Failed;
            reason = format!("round limit {} reached", next.policy.max_rounds);
        } else if next.stalls >= next.policy.max_stalls {
            next.status = GoalStatus::Failed;
            reason = format!("stall limit {} reached", next.policy.max_stalls);
        }
    }

    GoalTransition {
        applied,
        goal: next,
        reason,
    }
}

fn apply_done(next: &mut GoalSnapshot, goal: &GoalSnapshot, verdict: &GoalVerdict) -> bool {
    let valid = (!verdict.evidence.is_empty()
        && verdict
            .evidence
            .iter()
            .all(|id| goal.known_ids.iter().any(|known| known == id)))
        || (!goal.evidence.is_empty() && goal.done == goal.total);
    next.rounds = next.rounds.saturating_add(1);
    if valid {
        for id in &verdict.evidence {
            if !next.evidence.contains(id) {
                next.evidence.push(id.clone());
            }
        }
        next.status = GoalStatus::Done;
        next.done = next.total;
        next.wait_until_ms = None;
        next.wait_started_ms = None;
    } else {
        next.stalls = next.stalls.saturating_add(1);
    }
    valid
}

fn apply_continue(next: &mut GoalSnapshot, goal: &GoalSnapshot, verdict: &GoalVerdict, now: i64) {
    next.rounds = next.rounds.saturating_add(1);
    let mut progress = false;
    for id in &verdict.evidence {
        if goal.known_ids.contains(id) && !next.evidence.contains(id) {
            next.evidence.push(id.clone());
            progress = true;
        }
    }
    if progress {
        next.stalls = 0;
        next.last_progress_ms = now;
        next.wait_started_ms = None;
    } else {
        next.stalls = next.stalls.saturating_add(1);
    }
    next.status = GoalStatus::Active;
    next.wait_until_ms = None;
}

fn apply_wait(next: &mut GoalSnapshot, verdict: &GoalVerdict, now: i64, reason: &mut String) {
    next.rounds = next.rounds.saturating_add(1);
    let limit = next.policy.wait_limit_minutes.max(1);
    let minutes = verdict.wait_minutes.unwrap_or(limit).min(limit);
    let limit_ms = i64::from(limit).saturating_mul(60_000);
    let start = next.wait_started_ms.unwrap_or(now);
    if now.saturating_sub(start) >= limit_ms {
        next.status = GoalStatus::Blocked;
        next.wait_until_ms = None;
        next.wait_started_ms = None;
        *reason = format!("wait limit of {limit} minutes exceeded");
    } else {
        next.status = GoalStatus::Active;
        next.wait_started_ms = Some(start);
        let target = now.saturating_add(i64::from(minutes).saturating_mul(60_000));
        next.wait_until_ms = Some(target.min(start.saturating_add(limit_ms)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(policy: GoalPolicy) -> GoalSnapshot {
        GoalSnapshot {
            id: "gol_1".to_owned(),
            objective: "make the workspace clippy clean".to_owned(),
            criteria: vec!["no warnings".to_owned(), "tests pass".to_owned()],
            done: 0,
            total: 2,
            status: GoalStatus::Active,
            rounds: 0,
            stalls: 0,
            evidence: Vec::new(),
            wait_until_ms: None,
            wait_started_ms: None,
            last_progress_ms: 0,
            known_ids: vec!["tc_1".to_owned(), "msg_2".to_owned()],
            policy,
        }
    }

    fn verdict(kind: GoalVerdictKind, evidence: &[&str]) -> GoalVerdict {
        GoalVerdict {
            verdict: Some(kind),
            reason: "judged".to_owned(),
            evidence: evidence.iter().map(|id| (*id).to_owned()).collect(),
            ..GoalVerdict::default()
        }
    }

    #[test]
    fn done_requires_matching_evidence() {
        let goal = goal(GoalPolicy::default());

        let rejected = apply_verdict(&goal, &verdict(GoalVerdictKind::Done, &["tc_404"]), 1_000);
        assert!(!rejected.applied);
        assert_eq!(rejected.goal.status, GoalStatus::Active);
        assert_eq!(rejected.goal.stalls, 1);
        assert!(rejected.reason.contains("evidence"));

        let accepted = apply_verdict(&goal, &verdict(GoalVerdictKind::Done, &["tc_1"]), 1_000);
        assert!(accepted.applied);
        assert_eq!(accepted.goal.status, GoalStatus::Done);
        assert_eq!(accepted.goal.done, 2);
        assert_eq!(accepted.goal.evidence, vec!["tc_1".to_owned()]);

        let empty = apply_verdict(&goal, &verdict(GoalVerdictKind::Done, &[]), 1_000);
        assert!(!empty.applied);
    }

    #[test]
    fn rounds_stalls_and_wait_limits() {
        let round_limited = goal(GoalPolicy {
            max_rounds: 2,
            max_stalls: 10,
            wait_limit_minutes: 30,
        });
        let first = apply_verdict(
            &round_limited,
            &verdict(GoalVerdictKind::Continue, &[]),
            1_000,
        );
        assert_eq!(first.goal.status, GoalStatus::Active);
        let second = apply_verdict(&first.goal, &verdict(GoalVerdictKind::Continue, &[]), 2_000);
        assert_eq!(second.goal.status, GoalStatus::Failed);
        assert!(second.reason.contains("round limit"));

        let stall_limited = goal(GoalPolicy {
            max_rounds: 50,
            max_stalls: 2,
            wait_limit_minutes: 30,
        });
        let one = apply_verdict(&stall_limited, &verdict(GoalVerdictKind::Continue, &[]), 0);
        assert_eq!(one.goal.stalls, 1);
        let two = apply_verdict(&one.goal, &verdict(GoalVerdictKind::Continue, &[]), 0);
        assert_eq!(two.goal.status, GoalStatus::Failed);
        assert!(two.reason.contains("stall limit"));

        let progress = apply_verdict(
            &one.goal,
            &verdict(GoalVerdictKind::Continue, &["tc_1"]),
            5_000,
        );
        assert_eq!(progress.goal.stalls, 0);
        assert_eq!(progress.goal.status, GoalStatus::Active);

        let waiting = goal(GoalPolicy::default());
        let waited = apply_verdict(
            &waiting,
            &GoalVerdict {
                wait_minutes: Some(30),
                ..verdict(GoalVerdictKind::Wait, &[])
            },
            0,
        );
        assert_eq!(waited.goal.status, GoalStatus::Active);
        assert_eq!(waited.goal.wait_until_ms, Some(30 * 60_000));
        let exhausted = apply_verdict(
            &waited.goal,
            &GoalVerdict {
                wait_minutes: Some(30),
                ..verdict(GoalVerdictKind::Wait, &[])
            },
            30 * 60_000,
        );
        assert_eq!(exhausted.goal.status, GoalStatus::Blocked);
        assert!(exhausted.reason.contains("wait limit"));
    }

    #[test]
    fn judge_prompt_lists_criteria_and_outcomes() {
        let goal = goal(GoalPolicy::default());
        let prompt = judge_prompt(
            &goal,
            &[RecentOutcome {
                id: "tc_1".to_owned(),
                tool: "shell".to_owned(),
                outcome: Some("exit 1".to_owned()),
            }],
        );
        assert!(prompt.contains("make the workspace clippy clean"));
        assert!(prompt.contains("no warnings"));
        assert!(prompt.contains("tc_1 [shell]: exit 1"));
        assert!(prompt.contains("\"verdict\""));
    }
}
