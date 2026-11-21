//! W3A regression tests: the observer budget-extension signal.
//!
//! Offline and deterministic: the repair kind parses on the wire, root budget
//! fields survive serde with absent-by-default behavior, and validation routes
//! a request only to a busy session.

use oc_observer::{
    ObserverAction, ObserverConfig, RepairKind, RootContext, RootInput, parse_verdict,
};
use serde_json::{Value, json};

fn cfg() -> ObserverConfig {
    ObserverConfig::default()
}

#[test]
fn request_budget_extension_parses_and_round_trips_on_the_wire() {
    let verdict = parse_verdict(
        r#"{"action":"repair","target":"ses_root","reason":"deep work needs more steps",
            "repair":{"kind":"request_budget_extension","args":{"steps":5}}}"#,
        &cfg(),
    )
    .expect("valid repair verdict");
    assert_eq!(verdict.action, ObserverAction::Repair);
    assert_eq!(
        verdict.repair.as_ref().and_then(|repair| repair.kind),
        Some(RepairKind::RequestBudgetExtension)
    );
    assert_eq!(
        RepairKind::RequestBudgetExtension.as_str(),
        "request_budget_extension"
    );
    let serialized = serde_json::to_value(verdict).expect("verdict serializes");
    assert_eq!(
        serialized["repair"]["kind"],
        Value::String("request_budget_extension".to_owned())
    );
}

#[test]
fn root_budget_fields_are_present_when_published_and_absent_when_not() {
    let context = RootContext {
        step_cap: Some(50),
        steps_remaining: Some(5),
        ..RootContext::default()
    };
    let serialized = serde_json::to_value(&context).expect("context serializes");
    assert_eq!(serialized["step_cap"], json!(50));
    assert_eq!(serialized["steps_remaining"], json!(5));

    let empty = serde_json::to_value(RootContext::default()).expect("empty context serializes");
    assert!(empty.get("step_cap").is_none());
    assert!(empty.get("steps_remaining").is_none());

    let input: RootInput = serde_json::from_value(json!({
        "id": "ses_root",
        "agent": "engineer",
        "step": 45,
        "stepCap": 50,
        "stepsRemaining": 5,
        "state": "busy"
    }))
    .expect("input with budget fields");
    assert_eq!(input.step_cap, Some(50));
    assert_eq!(input.steps_remaining, Some(5));

    let old: RootInput = serde_json::from_value(json!({
        "id": "ses_root", "agent": "engineer", "step": 45, "state": "busy"
    }))
    .expect("legacy wire without budget fields");
    assert_eq!(old.step_cap, None);
    assert_eq!(old.steps_remaining, None);
}

#[test]
fn budget_request_without_a_busy_target_is_not_a_valid_verdict_shape() {
    // A missing target is rejected by the verdict parser before validation;
    // the engine-level busy-target rule is covered by validate_repair's own
    // harness. This pins the parser side of that contract.
    let parsed = parse_verdict(
        r#"{"action":"repair","reason":"extension",
            "repair":{"kind":"request_budget_extension"}}"#,
        &cfg(),
    );
    assert!(parsed.is_err());
}
