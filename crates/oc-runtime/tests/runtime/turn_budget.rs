//! Step-budget extension at the turn limit: the deterministic evaluator runs
//! over in-memory tool evidence and either grants extra steps (tools re-enabled,
//! durable `session.execution.step_extended`) or leaves the legacy
//! `StepLimited` path untouched (`session.execution.step_limit` once, canonical
//! qualifier text). See `docs/24-step-budget.md`.
//!
//! These are scenario tests over the published events and the durable rows; the
//! policy itself is unit-tested in `src/turn/budget.rs`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::support::*;

/// One scripted `read` request per entry, so the model calls the tool once on
/// each step it is allowed to use tools.
fn reads(paths: &[&str]) -> Vec<Script> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            tool_script(
                &format!("call_{index}"),
                json!({"path": path, "expectation": expectation()}),
            )
        })
        .collect()
}

fn data_keys(data: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = data
        .as_object()
        .expect("payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn small_cap_is_never_extended() {
    // `steps: 2` is below the extension floor, so the evaluator is never
    // consulted: the historical three-call sequence and the two-key qualifier
    // must stay byte-identical to the pre-extension behaviour.
    let harness = Harness::new(
        reads(&["hello.txt"; 3]),
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("keep going").await;

    assert_eq!(harness.model.calls(), 3);
    let requests = harness.model.requests();
    assert_eq!(
        requests[2].options.tool_choice.as_deref(),
        Some("none"),
        "the budget, not the model, ended the turn"
    );
    assert_eq!(
        requests[2].tools, requests[0].tools,
        "tool definitions stay on the final call so the provider prompt cache stays valid"
    );

    let step_limit = harness.sink.data("session.execution.step_limit");
    assert_eq!(
        step_limit.len(),
        1,
        "the exhausted step budget is reported exactly once: {step_limit:?}"
    );
    assert_eq!(step_limit[0]["steps"], 2);
    assert_eq!(step_limit[0]["sessionID"], harness.session);
    assert_eq!(
        data_keys(&step_limit[0]),
        ["sessionID", "steps"],
        "the legacy qualifier payload is unchanged: {}",
        step_limit[0]
    );

    assert!(
        !harness.sink.has("session.execution.step_extended"),
        "a cap below the extension floor is never extended"
    );
    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    assert!(
        !logged
            .iter()
            .any(|event| event.kind == "session.execution.step_extended"),
        "no durable extension event is written either"
    );
}

#[tokio::test]
async fn progress_extensions_continue_the_turn() {
    // `steps: 10` with a tool that completes on every step: at the limit the
    // evaluator sees ten `Completed` reads and grants a fresh batch, the next
    // request runs with tools enabled again, and the model then answers so the
    // turn is *not* step-limited.
    let mut scripts = reads(&["hello.txt"; 10]);
    scripts.push(text_script("finishing after the extension")); // tool-less limit call -> grant
    scripts.extend(reads(&["hello.txt"])); // first extended step
    scripts.push(text_script("all done"));
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 10}}}),
        None,
    )
    .await;
    harness.prompt("keep going").await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        13,
        "10 budgeted steps, one tool-less limit call, one extended step, one answer"
    );
    assert_eq!(
        requests[10].options.tool_choice.as_deref(),
        Some("none"),
        "the limit is reached through the usual tool-less request"
    );
    assert_eq!(
        requests[11].options.tool_choice, None,
        "the granted step runs with tools enabled again"
    );

    let extended = harness.sink.data("session.execution.step_extended");
    assert_eq!(extended.len(), 1, "one grant: {extended:?}");
    let grant = &extended[0];
    assert_eq!(
        data_keys(grant),
        ["grantIndex", "reason", "sessionID", "steps", "totalSteps"],
        "the extension payload has exactly the five documented keys: {grant}"
    );
    assert_eq!(grant["sessionID"], harness.session);
    assert_eq!(grant["grantIndex"], 1);
    assert_eq!(grant["steps"], 15, "EXTENSION_STEPS");
    let steps = grant["steps"].as_u64().expect("steps is a number");
    let total = grant["totalSteps"]
        .as_u64()
        .expect("totalSteps is a number");
    assert!(total > steps, "the grant grows the cap: {total} > {steps}");
    assert_eq!(total, 25, "the initial cap (10) plus the grant (15)");
    assert_eq!(grant["reason"], "recent tool progress");
    assert!(
        !grant.to_string().contains("null"),
        "the payload never emits null: {grant}"
    );

    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    let durable = logged
        .iter()
        .find(|event| event.kind == "session.execution.step_extended")
        .expect("durable extension event");
    assert_eq!(durable.version, 1);
    assert_eq!(data_keys(&durable.data), data_keys(grant));
    assert_eq!(durable.data["totalSteps"], total);
    let succeeded = logged
        .iter()
        .find(|event| event.kind == "session.execution.succeeded")
        .expect("succeeded terminal");
    assert!(
        durable.seq < succeeded.seq,
        "the grant is durable before the terminal it outlives: {} < {}",
        durable.seq,
        succeeded.seq
    );

    assert_eq!(
        harness.sink.count("session.execution.step_limit"),
        0,
        "an extended turn that the model ended is not qualified as step-limited"
    );
    assert!(
        !logged
            .iter()
            .any(|event| event.kind == "session.execution.step_limit"),
        "no durable step-limit qualifier either"
    );
    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(session["outcome"], "succeeded");
}

#[tokio::test]
async fn thrashing_is_not_extended() {
    // `steps: 10`, alternating a failing read with a successful one. The
    // trailing window therefore *does* contain successes, so only the thrash
    // rule (five identical `(name, message)` failures) can deny. A grant here
    // would emit an extension event; the legacy path must run instead with its
    // canonical qualifier text.
    let mut paths: Vec<&str> = Vec::new();
    for _ in 0..5 {
        paths.push("missing.txt");
        paths.push("hello.txt");
    }
    let mut scripts = reads(&paths);
    scripts.push(text_script("summarising the failures"));
    let harness = Harness::new(
        scripts,
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 10}}}),
        None,
    )
    .await;
    harness.prompt("keep trying").await;

    assert_eq!(
        harness.model.calls(),
        11,
        "10 budgeted steps plus the tool-less limit call"
    );
    assert_eq!(
        harness.model.requests()[10].options.tool_choice.as_deref(),
        Some("none")
    );
    assert_eq!(
        harness.sink.count("session.tool.failed"),
        5,
        "the fixture must actually produce the repeated identical errors"
    );
    assert_eq!(harness.sink.count("session.tool.success"), 5);

    assert!(
        !harness.sink.has("session.execution.step_extended"),
        "repeated identical failures must not buy more steps"
    );
    let step_limit = harness.sink.data("session.execution.step_limit");
    assert_eq!(
        step_limit.len(),
        1,
        "the qualifier is published exactly once: {step_limit:?}"
    );
    assert_eq!(step_limit[0]["steps"], 10);
    assert_eq!(data_keys(&step_limit[0]), ["sessionID", "steps"]);

    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(session["outcome"], "step_limit");
    let node = harness
        .store
        .session_node(&harness.session)
        .await
        .expect("session node")
        .expect("node row");
    assert_eq!(
        node.idle_reason.as_deref(),
        Some("step limit reached after 10 steps"),
        "the unchanged canonical step-limit grammar"
    );
}

#[tokio::test]
async fn extension_is_bounded() {
    // `steps: 10` with a model that would keep succeeding forever: every script
    // is a successful read, so tool evidence never stops looking like progress.
    // The turn is extended, but never without bound, and it still ends through
    // the legacy step-limit path.
    let harness = Harness::new(
        reads(&["hello.txt"; 80]),
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 10}}}),
        None,
    )
    .await;
    harness.prompt("never stop").await;

    let extensions = harness.sink.count("session.execution.step_extended");
    assert!(
        (1..=2).contains(&extensions),
        "the turn is extended, and bounded by MAX_EXTENSIONS (2); saw {extensions}"
    );
    let granted: u64 = harness
        .sink
        .data("session.execution.step_extended")
        .iter()
        .map(|grant| grant["steps"].as_u64().expect("steps is a number"))
        .sum();
    assert!(granted > 0, "a grant must buy at least one step");
    // 10 budgeted steps, then every granted step actually ran, and each grant
    // (plus the final deny) cost one tool-less limit request.
    assert_eq!(
        u64::try_from(harness.model.calls()).expect("call count fits"),
        10 + granted + u64::try_from(extensions).expect("extension count fits") + 1,
        "each granted step was actually requested before the budget bound again"
    );

    assert_eq!(
        harness.sink.count("session.execution.step_limit"),
        1,
        "a bounded turn still terminates through the step-limit qualifier"
    );
    let step_limit = harness.sink.data("session.execution.step_limit");
    assert_eq!(
        step_limit[0]["steps"], 10,
        "the qualifier reports the configured budget, not the extended cap"
    );
    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(
        session["outcome"], "step_limit",
        "the stored outcome is honest about why the turn ended: {session}"
    );
    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    assert_eq!(
        logged
            .iter()
            .filter(|event| event.kind == "session.execution.step_extended")
            .count(),
        extensions,
        "every grant is durable exactly once"
    );
}
