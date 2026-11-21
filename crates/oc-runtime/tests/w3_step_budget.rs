//! W3A regression: early-warning text and `MAX_STEPS_PROMPT` grammar.
//! Pure string assertions, offline and deterministic.

use oc_runtime::prompt::{MAX_STEPS_PROMPT, STEP_BUDGET_WARNING};

#[test]
fn early_warning_offers_the_observer_extension_path_and_keeps_tools_enabled() {
    assert!(STEP_BUDGET_WARNING.contains("goal_update"));
    assert!(STEP_BUDGET_WARNING.contains("observer"));
    assert!(STEP_BUDGET_WARNING.contains("request_budget_extension"));
    assert!(STEP_BUDGET_WARNING.contains("Tools remain enabled"));
}

#[test]
fn final_step_prompt_disables_tools_and_denies_a_promise_of_retry() {
    assert!(MAX_STEPS_PROMPT.contains("Do not call any more tools"));
    assert!(MAX_STEPS_PROMPT.contains("policy was consulted"));
    assert!(!MAX_STEPS_PROMPT.contains("will retry"));
}
