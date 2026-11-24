//! Durable replay of `session.execution.step_extended`.
//!
//! The step-budget grant is a turn-loop decision, but the *effective cap* is not
//! process state: [`replay`] re-derives it from the committed event log at
//! `run_steps` entry, so the durable grant event is a projection of committed
//! state instead of a decoration.
//!
//! # The window
//!
//! A session's log holds the grants of every turn it ever ran. A grant belongs
//! to the *open* execution window, defined as the events after the session's
//! last durable terminal execution event
//! (`session.execution.{succeeded,failed,interrupted}`). A settled turn closes
//! the window, so the next turn starts at `agent.steps` and inherits nothing; a
//! process that died mid-turn never published a terminal, left the window open,
//! and exactly then the recorded grant still applies.
//!
//! `session.execution.started` deliberately does not open the window: the turn
//! loop publishes it *before* `run_steps` runs, so keying on it would erase the
//! grants a resumed execution has to inherit.
//!
//! # Clamp
//!
//! Recorded grants are clamped by the *current* `step_budget` policy
//! (`max_extensions` and `ceiling_factor`). A configuration that shrank between
//! the grant and the replay therefore narrows the budget instead of widening it:
//! every rule fails closed.

//! # Cost
//!
//! The replay is a single forward scan of the session's `(aggregate_id, seq)`
//! index, and the store caps a page at 1000 events. It runs once per `run_steps`
//! entry (one turn), not once per step, so its cost is bounded by the size of the
//! session log rather than by the budget it reconstructs. A session that never
//! crashed has a short window, but the scan still has to reach the tail of the
//! log to find the last terminal; that is the price of not adding a
//! `seq > ?` reverse query to `oc-store` (not this change's crate).

use serde_json::Value;

use crate::runtime::{Runtime, RuntimeError};

use super::budget::BudgetPolicy;

/// Durable execution events that close an execution window.
const TERMINALS: [&str; 3] = [
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
];

/// The grant event this module replays.
const GRANT: &str = "session.execution.step_extended";

/// Page size of the log scan; the store caps a page at 1000 events.
const PAGE: usize = 1_000;

/// Grants that still apply to the session's open execution window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ReplayedGrants {
    /// Granted step counts, in grant order.
    pub(super) steps: Vec<usize>,
    /// Effective cap: the initial cap plus every applied grant.
    pub(super) cap: usize,
    /// Recorded grants the current policy no longer allows.
    pub(super) dropped: usize,
}

/// Re-derive the effective step cap from the durable grant log.
pub(super) async fn replay(
    runtime: &Runtime,
    session: &str,
    initial_cap: usize,
    policy: &BudgetPolicy,
) -> Result<ReplayedGrants, RuntimeError> {
    let recorded = recorded_steps(runtime, session).await?;
    Ok(effective(&recorded, initial_cap, policy))
}

/// The `steps` values of the grants in the open window, oldest first.
async fn recorded_steps(runtime: &Runtime, session: &str) -> Result<Vec<usize>, RuntimeError> {
    let store = &runtime.inner.config.store;
    let mut recorded: Vec<usize> = Vec::new();
    let mut after = 0_i64;
    loop {
        let events = store.events_after(session, after, PAGE).await?;
        let Some(last) = events.last() else {
            return Ok(recorded);
        };
        after = last.seq;
        for event in &events {
            if TERMINALS.contains(&event.kind.as_str()) {
                // A terminal closes the window: the grants before it belong to a
                // turn that already settled and must not be inherited.
                recorded.clear();
                continue;
            }
            if event.kind != GRANT {
                continue;
            }
            let Some(steps) = grant_steps(&event.data) else {
                tracing::warn!(
                    session = %session,
                    event = %event.id,
                    "step grant carries no usable `steps` value; ignoring it"
                );
                continue;
            };
            recorded.push(steps);
        }
    }
}

/// The granted step count of one `session.execution.step_extended` payload.
fn grant_steps(data: &Value) -> Option<usize> {
    let steps = data.get("steps").and_then(Value::as_u64)?;
    let steps = usize::try_from(steps).ok()?;
    (steps > 0).then_some(steps)
}

/// Apply the recorded grants to `initial_cap`, clamped by `policy`.
///
/// The sum is the authority, not the `totalSteps` field of a single event: a
/// grant's `totalSteps` is derivable from `initial_cap` plus the grants before
/// it, so a log whose grants were recorded against a different `agent.steps`
/// still replays without trusting one stale total.
fn effective(recorded: &[usize], initial_cap: usize, policy: &BudgetPolicy) -> ReplayedGrants {
    let ceiling = initial_cap.saturating_mul(policy.ceiling_factor);
    let mut steps: Vec<usize> = Vec::new();
    let mut granted = 0_usize;
    for &grant in recorded {
        if steps.len() >= policy.max_extensions || granted.saturating_add(grant) > ceiling {
            break;
        }
        granted = granted.saturating_add(grant);
        steps.push(grant);
    }
    let cap = steps
        .iter()
        .fold(initial_cap, |cap, grant| cap.saturating_add(*grant));
    ReplayedGrants {
        dropped: recorded.len().saturating_sub(steps.len()),
        steps,
        cap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_window_replays_to_the_initial_cap() {
        let replayed = effective(&[], 50, &BudgetPolicy::default());
        assert_eq!(replayed.cap, 50);
        assert!(replayed.steps.is_empty());
        assert_eq!(replayed.dropped, 0);
    }

    #[test]
    fn recorded_grants_raise_the_cap_in_order() {
        let replayed = effective(&[15, 5], 10, &BudgetPolicy::default());
        assert_eq!(replayed.steps, vec![15, 5]);
        assert_eq!(replayed.cap, 30, "10 + 15 + 5");
        assert_eq!(replayed.dropped, 0);
    }

    #[test]
    fn a_shrunk_extension_limit_narrows_the_budget() {
        // `max_extensions` default is 2: a third recorded grant is dropped.
        let replayed = effective(&[5, 5, 5], 10, &BudgetPolicy::default());
        assert_eq!(replayed.steps, vec![5, 5]);
        assert_eq!(replayed.cap, 20);
        assert_eq!(replayed.dropped, 1);
    }

    #[test]
    fn a_shrunk_ceiling_factor_narrows_the_budget() {
        // Under the shipped defaults `max_extensions` is 2 and the ceiling is
        // `10 * 2 = 20`, so four recorded grants of 5 replay as two grants.
        let defaults = effective(&[5, 5, 5, 5], 10, &BudgetPolicy::default());
        assert_eq!(defaults.steps, vec![5, 5]);
        assert_eq!(defaults.cap, 20, "10 + 5 + 5");
        assert_eq!(defaults.dropped, 2);

        // Ceiling dimension, isolated: `max_extensions: 8` cannot bind here, so
        // only `granted + grant <= initial_cap * ceiling_factor` can truncate.
        // The boundary is inclusive: the ceiling of 20 is spent exactly and the
        // next grant of 5 is refused.
        let roomy = BudgetPolicy {
            max_extensions: 8,
            ..BudgetPolicy::default()
        };
        let full = effective(&[5, 5, 5, 5, 5], 10, &roomy);
        assert_eq!(full.steps, vec![5, 5, 5, 5], "the ceiling of 20 is spent");
        assert_eq!(full.cap, 30);
        assert_eq!(full.dropped, 1);

        // The shrunk factor halves that ceiling to 10: the same log replays
        // strictly narrower, which is the regression this test exists for.
        let tighter = BudgetPolicy {
            ceiling_factor: 1,
            ..roomy
        };
        let shrunk = effective(&[5, 5, 5, 5, 5], 10, &tighter);
        assert_eq!(
            shrunk.steps,
            vec![5, 5],
            "a ceiling of 10 admits two grants"
        );
        assert_eq!(shrunk.cap, 20);
        assert_eq!(shrunk.dropped, 3);
        assert!(
            shrunk.cap < full.cap,
            "a shrunken ceiling must narrow the budget, never widen it"
        );

        // Count dimension, isolated: the ceiling is raised to 1000, so only the
        // `max_extensions` rule can explain why five recorded grants become two.
        let count_bound = BudgetPolicy {
            ceiling_factor: 100,
            ..BudgetPolicy::default()
        };
        let capped = effective(&[5, 5, 5, 5, 5], 10, &count_bound);
        assert_eq!(capped.steps, vec![5, 5], "the default cap is two grants");
        assert_eq!(capped.cap, 20);
        assert_eq!(capped.dropped, 3);
    }

    #[test]
    fn a_zero_or_missing_grant_is_not_a_grant() {
        assert_eq!(grant_steps(&serde_json::json!({"steps": 0})), None);
        assert_eq!(grant_steps(&serde_json::json!({"steps": "5"})), None);
        assert_eq!(grant_steps(&serde_json::json!({"totalSteps": 25})), None);
        assert_eq!(grant_steps(&serde_json::json!({"steps": 7})), Some(7));
    }
}
