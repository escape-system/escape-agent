//! Deterministic step-budget extension policy.
//!
//! When a turn reaches its step limit the runtime may grant a bounded number of
//! extra steps, but only when the in-memory tool evidence shows real progress.
//! No model call, no IO, no clock: the decision is a pure function of observed
//! tool outcomes so it is reproducible and replay-safe.
//!
//! See `docs/29-step-budget.md` for the policy rationale and the observability
//! contract.

use oc_types::message::{AssistantPart, ToolState};
use serde_json::Value;

/// Tunable step-budget policy, read from `step_budget.*` in merged config.
///
/// Defaults are the historical constants, so an absent section keeps every
/// existing turn byte-identical. All fields are fail-closed: a bad value falls
/// back to its default rather than loosening the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetPolicy {
    pub(crate) extension_steps: usize,
    pub(super) max_extensions: usize,
    pub(super) min_cap: usize,
    pub(super) thrash_limit: usize,
    pub(super) window: usize,
    pub(super) ceiling_factor: usize,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            extension_steps: EXTENSION_STEPS,
            max_extensions: MAX_EXTENSIONS,
            min_cap: MIN_CAP_FOR_EXTENSION,
            thrash_limit: THRASH_LIMIT,
            window: WINDOW,
            ceiling_factor: CEILING_FACTOR,
        }
    }
}

impl BudgetPolicy {
    /// Read overrides from a merged config value. Unknown keys and malformed
    /// values are ignored so a partial/typo'd section cannot widen the budget.
    pub(crate) fn from_merged(merged: &Value) -> Self {
        let defaults = Self::default();
        let Some(section) = merged.get("step_budget").and_then(Value::as_object) else {
            return defaults;
        };
        let mut policy = defaults;
        for (key, value) in section {
            let Some(number) = value.as_u64().and_then(|n| usize::try_from(n).ok()) else {
                continue;
            };
            let (minimum, field) = match key.as_str() {
                "extension_steps" => (1, &mut policy.extension_steps),
                "max_extensions" => (0, &mut policy.max_extensions),
                "min_cap" => (1, &mut policy.min_cap),
                "thrash_limit" => (1, &mut policy.thrash_limit),
                "window" => (1, &mut policy.window),
                "ceiling_factor" => (1, &mut policy.ceiling_factor),
                _ => continue,
            };
            if number >= minimum {
                *field = number;
            }
        }
        policy
    }
}

/// Extra steps granted per extension.
pub(super) const EXTENSION_STEPS: usize = 15;
/// Hard cap on the number of grants per turn.
pub(super) const MAX_EXTENSIONS: usize = 2;
/// Caps below this value are never extended.
pub(super) const MIN_CAP_FOR_EXTENSION: usize = 10;
/// Identical `(name, message)` failures reaching this count are thrashing.
pub(super) const THRASH_LIMIT: usize = 3;
/// How many trailing observations count as "recent".
pub(super) const WINDOW: usize = 5;
/// Total granted extension may never exceed `initial_cap * CEILING_FACTOR`.
pub(super) const CEILING_FACTOR: usize = 2;

/// Outcome of a single observed tool call, reduced to what the policy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Observation {
    Ok,
    Err(String),
    Other,
}

/// One observed tool call in the order it appeared in the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolObservation {
    pub(super) name: String,
    pub(super) outcome: Observation,
}

/// Result of evaluating the policy for one step-limit boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BudgetDecision {
    pub(super) grant: Option<usize>,
    pub(super) reason: &'static str,
}

impl BudgetDecision {
    fn deny(reason: &'static str) -> Self {
        Self {
            grant: None,
            reason,
        }
    }

    fn grant_with(reason: &'static str, steps: usize) -> Self {
        Self {
            grant: Some(steps),
            reason,
        }
    }
}

/// Policy-aware evaluator. First matching rule wins; every rule is fail-closed.
///
/// Thrash detection only inspects the trailing `2 * window` observations so
/// that a deep run with sequential, distinct failures and intermittent success
/// can still qualify as progress; a genuine thrash loop stays inside that
/// window and is still denied.
pub(super) fn evaluate_with_policy(
    observations: &[ToolObservation],
    tool_calls: usize,
    grants: &[usize],
    initial_cap: usize,
    policy: &BudgetPolicy,
) -> BudgetDecision {
    if grants.len() >= policy.max_extensions {
        return BudgetDecision::deny("extension limit reached");
    }
    let granted: usize = grants.iter().copied().sum();
    if granted.saturating_add(policy.extension_steps)
        > initial_cap.saturating_mul(policy.ceiling_factor)
    {
        return BudgetDecision::deny("extension ceiling reached");
    }
    if tool_calls == 0 {
        return BudgetDecision::deny("no tool calls in this turn");
    }
    if is_thrashing(observations, policy) {
        return BudgetDecision::deny("repeated identical tool failure");
    }
    if !has_recent_success(observations, policy) {
        return BudgetDecision::deny("no recent successful tool call");
    }
    BudgetDecision::grant_with("recent tool progress", policy.extension_steps)
}

fn is_thrashing(observations: &[ToolObservation], policy: &BudgetPolicy) -> bool {
    let recent = observations
        .len()
        .saturating_sub(policy.window.saturating_mul(2));
    for (index, observation) in observations.iter().enumerate().skip(recent) {
        let Observation::Err(message) = &observation.outcome else {
            continue;
        };
        let mut same = 1_usize;
        for other in observations.iter().skip(index + 1) {
            if other.name == observation.name
                && matches!(&other.outcome, Observation::Err(existing) if existing == message)
            {
                same += 1;
                if same >= policy.thrash_limit {
                    return true;
                }
            }
        }
    }
    false
}

fn has_recent_success(observations: &[ToolObservation], policy: &BudgetPolicy) -> bool {
    observations
        .iter()
        .rev()
        .take(policy.window)
        .any(|observation| observation.outcome == Observation::Ok)
}

/// True when `grants`, `cap`, and `initial_cap` leave room for an automatic
/// extension under `policy`.
pub(super) fn can_extend_with_policy(
    grants: &[usize],
    initial_cap: usize,
    cap: usize,
    policy: &BudgetPolicy,
) -> bool {
    if cap < policy.min_cap || grants.len() >= policy.max_extensions {
        return false;
    }
    let granted: usize = grants.iter().copied().sum();
    granted.saturating_add(policy.extension_steps)
        <= initial_cap.saturating_mul(policy.ceiling_factor)
}

/// How many additional steps `initial_cap` may still receive under `policy`.
pub(super) fn remaining_ceiling_with_policy(
    grants: &[usize],
    initial_cap: usize,
    policy: &BudgetPolicy,
) -> usize {
    if grants.len() >= policy.max_extensions {
        return 0;
    }
    let granted: usize = grants.iter().copied().sum();
    initial_cap
        .saturating_mul(policy.ceiling_factor)
        .saturating_sub(granted)
}

/// Append the tool parts of one finished step to the turn-wide evidence log.
///
/// The step that reaches the limit is a `final_call`: it is streamed with tools
/// disabled and the streamer drops tool events in that mode, so its content
/// carries text only. Evidence therefore has to be collected as each tool-using
/// step completes, which also matches "what did the turn actually do".
pub(super) fn observe(accumulated: &mut Vec<ToolObservation>, parts: &[AssistantPart]) {
    for part in parts {
        let AssistantPart::Tool(tool) = part else {
            continue;
        };
        let outcome = match &tool.state {
            ToolState::Completed {
                input, metadata, ..
            } => {
                if tool.name == "shell"
                    && let Some(reason) = super::verify::shell_failure_reason(metadata.as_ref())
                {
                    let cmd = input
                        .get("command")
                        .and_then(Value::as_str)
                        .map(|raw| {
                            raw.split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .chars()
                                .take(80)
                                .collect::<String>()
                        })
                        .filter(|s| !s.is_empty());
                    let detail = match cmd {
                        Some(cmd) => format!("{cmd}: {reason}"),
                        None => reason,
                    };
                    Observation::Err(detail)
                } else {
                    Observation::Ok
                }
            }
            ToolState::Error { error, .. } => Observation::Err(error.message.clone()),
            ToolState::Streaming { .. } | ToolState::Running { .. } => Observation::Other,
        };
        accumulated.push(ToolObservation {
            name: tool.name.clone(),
            outcome,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str) -> ToolObservation {
        ToolObservation {
            name: name.to_owned(),
            outcome: Observation::Ok,
        }
    }

    fn err(name: &str, message: &str) -> ToolObservation {
        ToolObservation {
            name: name.to_owned(),
            outcome: Observation::Err(message.to_owned()),
        }
    }

    fn other(name: &str) -> ToolObservation {
        ToolObservation {
            name: name.to_owned(),
            outcome: Observation::Other,
        }
    }

    #[test]
    fn grants_on_recent_progress() {
        let observations = [ok("read"), err("shell", "boom"), ok("edit")];
        let decision = evaluate_with_policy(&observations, 3, &[], 10, &BudgetPolicy::default());
        assert_eq!(decision.grant, Some(EXTENSION_STEPS));
        assert_eq!(decision.reason, "recent tool progress");
    }

    #[test]
    fn denies_thrashing_on_identical_failures() {
        let observations = [
            err("shell", "command not found: foo"),
            ok("read"),
            err("shell", "command not found: foo"),
            err("shell", "command not found: foo"),
        ];
        let decision = evaluate_with_policy(&observations, 4, &[], 10, &BudgetPolicy::default());
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "repeated identical tool failure");
    }

    #[test]
    fn denies_when_no_tool_calls() {
        let observations = [ok("read")];
        let decision = evaluate_with_policy(&observations, 0, &[], 10, &BudgetPolicy::default());
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "no tool calls in this turn");
    }

    #[test]
    fn denies_when_window_has_no_success() {
        let observations = [
            ok("read"),
            other("glob"),
            other("grep"),
            other("edit"),
            other("read"),
            other("glob"),
            other("grep"),
        ];
        let decision = evaluate_with_policy(&observations, 7, &[], 10, &BudgetPolicy::default());
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "no recent successful tool call");
    }

    #[test]
    fn denies_at_extension_limit() {
        let observations = [ok("read")];
        let grants = [EXTENSION_STEPS; MAX_EXTENSIONS];
        let decision =
            evaluate_with_policy(&observations, 1, &grants, 100, &BudgetPolicy::default());
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "extension limit reached");
    }

    #[test]
    fn denies_when_ceiling_would_be_exceeded() {
        // Ceiling rule: granted_so_far + EXTENSION_STEPS must not exceed
        // initial_cap * CEILING_FACTOR. At the smallest cap the caller will
        // ever evaluate (MIN_CAP_FOR_EXTENSION), one prior grant already
        // exhausts the headroom:
        //   granted_so_far (15) + EXTENSION_STEPS (15) = 30
        //   initial_cap (10) * CEILING_FACTOR (2)     = 20  => deny.
        // Fixture is const-derived so it cannot drift out of the deny region
        // without this test failing.
        let initial_cap = MIN_CAP_FOR_EXTENSION;
        let grants = [EXTENSION_STEPS];
        let observations = [ok("read"), ok("edit")];
        let decision = evaluate_with_policy(
            &observations,
            2,
            &grants,
            initial_cap,
            &BudgetPolicy::default(),
        );
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "extension ceiling reached");
    }

    #[test]
    fn caller_enforces_min_cap_floor() {
        // Caller contract: the evaluator has no MIN_CAP_FOR_EXTENSION floor of
        // its own. The caller (`run_steps` in `mod.rs`) must refuse to call
        // the evaluator when the current cap is below `MIN_CAP_FOR_EXTENSION`, so
        // small deterministic caps (2, 5) keep the legacy behaviour
        // byte-identical. At the floor itself the evaluator must grant.
        let observations = [ok("read")];
        let decision = evaluate_with_policy(
            &observations,
            1,
            &[],
            MIN_CAP_FOR_EXTENSION,
            &BudgetPolicy::default(),
        );
        assert_eq!(decision.grant, Some(EXTENSION_STEPS));
        assert_eq!(decision.reason, "recent tool progress");
    }

    #[test]
    fn deepening_with_distinct_sequential_failures_is_granted() {
        // A long run with distinct failures early and a recent success must not
        // be denied by the old unbounded thrash scan.
        let mut observations: Vec<ToolObservation> = Vec::new();
        for index in 0..12 {
            observations.push(err("shell", &format!("failure {index}")));
        }
        observations.push(ok("read"));
        observations.push(err("shell", "current failure"));
        observations.push(ok("edit"));
        let decision = evaluate_with_policy(
            &observations,
            observations.len(),
            &[],
            50,
            &BudgetPolicy::default(),
        );
        assert_eq!(decision.grant, Some(EXTENSION_STEPS));
        assert_eq!(decision.reason, "recent tool progress");
    }

    #[test]
    fn configured_policy_changes_grant_size_and_limit() {
        let policy = BudgetPolicy {
            extension_steps: 7,
            max_extensions: 1,
            min_cap: 10,
            thrash_limit: 3,
            window: 5,
            ceiling_factor: 4,
        };
        let decision = evaluate_with_policy(&[ok("read")], 1, &[], 20, &policy);
        assert_eq!(decision.grant, Some(7));
        let denied = evaluate_with_policy(&[ok("read")], 1, &[7], 20, &policy);
        assert_eq!(denied.grant, None);
        assert_eq!(denied.reason, "extension limit reached");
    }

    #[test]
    fn merged_config_reads_step_budget_overrides() {
        let config = serde_json::json!({
            "step_budget": {
                "extension_steps": 9,
                "max_extensions": 3,
                "min_cap": 12,
                "thrash_limit": 2,
                "window": 6,
                "ceiling_factor": 3,
                "extension_steps_bad": "ignored"
            }
        });
        assert_eq!(
            BudgetPolicy::from_merged(&config),
            BudgetPolicy {
                extension_steps: 9,
                max_extensions: 3,
                min_cap: 12,
                thrash_limit: 2,
                window: 6,
                ceiling_factor: 3,
            }
        );
        assert_eq!(
            BudgetPolicy::from_merged(&serde_json::json!({})),
            BudgetPolicy::default()
        );
    }

    #[test]
    fn failing_shell_completed_results_count_as_errors_and_detect_thrashing() {
        use oc_types::message::{ToolPart, ToolTime};
        use serde_json::{Map, json};

        let mut fail_meta = Map::new();
        fail_meta.insert("status".to_owned(), json!("completed"));
        fail_meta.insert("exit".to_owned(), json!(1));

        let part = AssistantPart::Tool(Box::new(ToolPart {
            id: "call_sh".to_owned(),
            name: "shell".to_owned(),
            executed: Some(true),
            native: None,
            time: ToolTime {
                created: 1,
                ran: Some(1),
                completed: Some(2),
            },
            state: ToolState::Completed {
                input: Map::new(),
                content: Vec::new(),
                metadata: Some(fail_meta),
            },
        }));
        let mut observations = Vec::new();
        for _ in 0..3 {
            observe(&mut observations, std::slice::from_ref(&part));
        }
        let decision = evaluate_with_policy(
            &observations,
            3,
            &[],
            MIN_CAP_FOR_EXTENSION,
            &BudgetPolicy::default(),
        );
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "repeated identical tool failure");
    }

    #[test]
    fn distinct_failed_shell_commands_do_not_trip_thrash_limit() {
        use oc_types::message::{ToolPart, ToolTime};
        use serde_json::{Map, json};

        let mut fail_meta = Map::new();
        fail_meta.insert("status".to_owned(), json!("completed"));
        fail_meta.insert("exit".to_owned(), json!(101));

        let mut observations = Vec::new();
        for cmd in ["cargo test alpha", "cargo test beta", "cargo test gamma"] {
            let mut input = Map::new();
            input.insert("command".to_owned(), json!(cmd));
            let part = AssistantPart::Tool(Box::new(ToolPart {
                id: "call_sh".to_owned(),
                name: "shell".to_owned(),
                executed: Some(true),
                native: None,
                time: ToolTime {
                    created: 1,
                    ran: Some(1),
                    completed: Some(2),
                },
                state: ToolState::Completed {
                    input,
                    content: Vec::new(),
                    metadata: Some(fail_meta.clone()),
                },
            }));
            observe(&mut observations, std::slice::from_ref(&part));
        }
        observations.push(ok("edit"));
        let decision = evaluate_with_policy(
            &observations,
            4,
            &[],
            MIN_CAP_FOR_EXTENSION,
            &BudgetPolicy::default(),
        );
        assert_eq!(decision.grant, Some(EXTENSION_STEPS));
    }
}
