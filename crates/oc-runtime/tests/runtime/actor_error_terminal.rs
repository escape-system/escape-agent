//! An `Err` that escapes the drain loop must not skip the turn's terminal
//! settlement: the rows the dead turn left open are repaired, and exactly one
//! `session.idle` / `session.execution.failed` pair is published afterwards.

use std::sync::Arc;
use std::time::Duration;

use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use serde_json::{Value, json};

use crate::support::*;

/// Panics inside the tool `JoinSet`, so `execute_tools` surfaces a `JoinError`
/// and the drain loop exits through `RuntimeError::Task`. The assistant row,
/// its tool part and the session busy flag are left open at that point.
struct PanicTool;

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &'static str {
        "panic_tool"
    }

    fn description(&self) -> &'static str {
        "Test-only tool that panics and kills the session actor mid-turn."
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        panic!("panic_tool exploded after the turn became busy");
    }
}

fn tool_part(message: &Value) -> &Value {
    message["content"]
        .as_array()
        .and_then(|parts| parts.iter().find(|part| part["type"] == "tool"))
        .unwrap_or_else(|| panic!("assistant row has no tool part: {message:?}"))
}

#[tokio::test]
// The scenario asserts the whole failure path (events, rows, inbox, respawn)
// as one narrative; splitting it would hide cross-step invariants.
#[allow(clippy::too_many_lines)]
async fn actor_error_publishes_one_terminal_pair_and_closes_the_rows() {
    let mut registry = ToolRegistry::with_defaults();
    registry.register(Arc::new(PanicTool));

    let harness = Harness::new_with_tools(
        vec![
            named_tool_script(
                "panic_tool",
                "call_panic",
                json!({"expectation": expectation()}),
            ),
            text_script("still alive"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
        Arc::new(registry),
    )
    .await;

    // First turn: the tool panic tears the actor down before `settle` runs.
    harness.enqueue("go").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");

    // The durable row the dead turn left behind is read before the actor's
    // failure finalizer rewrites it, so wait for the terminal pair instead of
    // for `wait_idle` (which only tracks the running flag).
    harness.wait_for_event("session.execution.failed").await;
    harness.wait_for_event("session.idle").await;

    assert_eq!(
        harness.sink.count("session.execution.failed"),
        1,
        "the dead turn must publish exactly one terminal outcome: {:?}",
        harness.sink.kinds()
    );
    assert_eq!(
        harness.sink.count("session.idle"),
        1,
        "the dead turn must publish exactly one idle event: {:?}",
        harness.sink.kinds()
    );
    assert_eq!(
        harness.sink.count("session.execution.started"),
        1,
        "the failing execution started exactly once"
    );
    assert!(
        !harness.sink.has("session.execution.succeeded"),
        "a dead turn cannot succeed: {:?}",
        harness.sink.kinds()
    );

    let failed = harness.sink.data("session.execution.failed");
    assert_eq!(failed[0]["sessionID"], harness.session.as_str());
    assert_eq!(
        failed[0]["error"]["type"], "InternalError",
        "a task panic is an internal failure: {:?}",
        failed[0]
    );
    assert!(
        failed[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "the failure carries a message: {:?}",
        failed[0]
    );

    // `session.idle` must be published only after the idle row committed, and
    // before the terminal outcome, exactly like the normal settle path.
    let kinds = harness.sink.kinds();
    let idle_at = kinds
        .iter()
        .position(|kind| kind == "session.idle")
        .expect("session.idle published");
    let failed_at = kinds
        .iter()
        .position(|kind| kind == "session.execution.failed")
        .expect("session.execution.failed published");
    assert!(
        idle_at < failed_at,
        "the terminal outcome follows the committed idle row: {kinds:?}"
    );

    // The dead turn's assistant and tool rows are closed, not left running.
    let messages = harness.messages().await;
    let dead_rows = assistants(&messages);
    assert_eq!(dead_rows.len(), 1, "one assistant row for the dead turn");
    let dead = dead_rows[0];
    assert_eq!(dead["finish"], "error", "the dead step is closed: {dead:?}");
    assert_eq!(dead["error"]["type"], "MessageAbortedError");
    assert!(
        dead.pointer("/time/completed").is_some_and(Value::is_i64),
        "the dead assistant row carries a completion stamp: {dead:?}"
    );
    let tool = tool_part(dead);
    assert_eq!(tool["name"], "panic_tool");
    assert_eq!(
        tool["state"]["status"], "error",
        "the in-flight tool row is settled: {tool:?}"
    );
    assert_eq!(tool["state"]["error"]["type"], "ToolAbortedError");
    assert!(
        tool.pointer("/time/completed").is_some_and(Value::is_i64),
        "the settled tool row carries a completion stamp: {tool:?}"
    );

    // The session is durably not busy and records the failed outcome.
    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(session["outcome"], "failed");
    assert!(
        session.pointer("/time/idle").is_some_and(Value::is_i64),
        "the session records when it became idle: {session:?}"
    );
    assert!(
        harness.inbox().await.is_empty(),
        "the inbox row was consumed"
    );

    // The terminal pair is published once, with nothing left to re-run.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(harness.sink.count("session.execution.failed"), 1);
    assert_eq!(harness.sink.count("session.idle"), 1);
    assert_eq!(harness.messages().await.len(), messages.len());

    // The actor exits, and a fresh one still serves the next prompt.
    harness.prompt("again").await;
    let after = harness.messages().await;
    let assistants_after = assistants(&after);
    assert_eq!(
        assistants_after.len(),
        2,
        "the next turn produced its own step"
    );
    let reply = assistants_after
        .iter()
        .find(|message| message["finish"] == "stop")
        .unwrap_or_else(|| panic!("no finished assistant reply after respawn: {after:?}"));
    assert_eq!(reply["content"][0]["text"], "still alive");
    assert_eq!(harness.sink.count("session.execution.succeeded"), 1);
    assert_eq!(
        harness.sink.count("session.execution.failed"),
        1,
        "the successful turn must not republish the old failure: {:?}",
        harness.sink.kinds()
    );
    assert_eq!(harness.sink.count("session.idle"), 2, "one idle per turn");
    assert_eq!(harness.model.calls(), 2);
}
