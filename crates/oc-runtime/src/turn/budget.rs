//! Deterministic step-budget extension policy.
//!
//! When a turn reaches its step limit the runtime may grant a bounded number of
//! extra steps, but only when the in-memory tool evidence shows real progress.
//! No model call, no IO, no clock: the decision is a pure function of observed
//! tool outcomes so it is reproducible and replay-safe.
//!
//! See `docs/24-step-budget.md` for the policy rationale and the observability
//! contract.

use oc_types::message::{AssistantPart, ToolState};

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

    fn grant(reason: &'static str) -> Self {
        Self {
            grant: Some(EXTENSION_STEPS),
            reason,
        }
    }
}

/// First matching rule wins; every rule is fail-closed.
///
/// The caller is responsible for the `MIN_CAP_FOR_EXTENSION` floor and for
/// rejecting a zero grant.
pub(super) fn evaluate(
    observations: &[ToolObservation],
    tool_calls: usize,
    grants: &[usize],
    initial_cap: usize,
) -> BudgetDecision {
    if grants.len() >= MAX_EXTENSIONS {
        return BudgetDecision::deny("extension limit reached");
    }
    let granted: usize = grants.iter().copied().sum();
    if granted.saturating_add(EXTENSION_STEPS) > initial_cap.saturating_mul(CEILING_FACTOR) {
        return BudgetDecision::deny("extension ceiling reached");
    }
    if tool_calls == 0 {
        return BudgetDecision::deny("no tool calls in this turn");
    }
    if is_thrashing(observations) {
        return BudgetDecision::deny("repeated identical tool failure");
    }
    if !has_recent_success(observations) {
        return BudgetDecision::deny("no recent successful tool call");
    }
    BudgetDecision::grant("recent tool progress")
}

fn is_thrashing(observations: &[ToolObservation]) -> bool {
    for (index, observation) in observations.iter().enumerate() {
        let Observation::Err(message) = &observation.outcome else {
            continue;
        };
        let mut same = 1_usize;
        for other in observations.iter().skip(index + 1) {
            if other.name == observation.name
                && matches!(&other.outcome, Observation::Err(existing) if existing == message)
            {
                same += 1;
                if same >= THRASH_LIMIT {
                    return true;
                }
            }
        }
    }
    false
}

fn has_recent_success(observations: &[ToolObservation]) -> bool {
    observations
        .iter()
        .rev()
        .take(WINDOW)
        .any(|observation| observation.outcome == Observation::Ok)
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
            ToolState::Completed { .. } => Observation::Ok,
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
        let decision = evaluate(&observations, 3, &[], 10);
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
        let decision = evaluate(&observations, 4, &[], 10);
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "repeated identical tool failure");
    }

    #[test]
    fn denies_when_no_tool_calls() {
        let observations = [ok("read")];
        let decision = evaluate(&observations, 0, &[], 10);
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
        let decision = evaluate(&observations, 7, &[], 10);
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "no recent successful tool call");
    }

    #[test]
    fn denies_at_extension_limit() {
        let observations = [ok("read")];
        let grants = [EXTENSION_STEPS; MAX_EXTENSIONS];
        let decision = evaluate(&observations, 1, &grants, 100);
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
        let decision = evaluate(&observations, 2, &grants, initial_cap);
        assert_eq!(decision.grant, None);
        assert_eq!(decision.reason, "extension ceiling reached");
    }

    #[test]
    fn caller_enforces_min_cap_floor() {
        // Caller contract: the evaluator has no MIN_CAP_FOR_EXTENSION floor of
        // its own. The caller (`run_steps` in `mod.rs`) must refuse to call
        // `evaluate` when the current cap is below `MIN_CAP_FOR_EXTENSION`, so
        // small deterministic caps (2, 5) keep the legacy behaviour
        // byte-identical. At the floor itself the evaluator must grant.
        let observations = [ok("read")];
        let decision = evaluate(&observations, 1, &[], MIN_CAP_FOR_EXTENSION);
        assert_eq!(decision.grant, Some(EXTENSION_STEPS));
        assert_eq!(decision.reason, "recent tool progress");
    }
}
