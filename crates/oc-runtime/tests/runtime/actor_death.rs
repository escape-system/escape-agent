//! A tool panic kills the session actor task. The runtime must respawn it on
//! the next notify instead of waking a dead handle and wedging the session.

use std::sync::Arc;
use std::time::Duration;

use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use serde_json::{Value, json};

use crate::support::*;

/// Always panics while running inside the tool `JoinSet`, which makes
/// `execute_tools` surface a `JoinError` and stops the actor loop with `Err`.
struct PanicTool;

#[async_trait::async_trait]
impl Tool for PanicTool {
    fn name(&self) -> &'static str {
        "panic_tool"
    }

    fn description(&self) -> &'static str {
        "Test-only tool that panics to kill the session actor."
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        panic!("panic_tool exploded inside the actor");
    }
}

#[tokio::test]
async fn tool_panic_does_not_wedge_the_session() {
    let mut registry = ToolRegistry::with_defaults();
    registry.register(Arc::new(PanicTool));

    let harness = Harness::new_with_tools(
        vec![
            named_tool_script(
                "panic_tool",
                "call_1",
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

    // The panic tears the actor down. The first turn is expected to wedge (its
    // idle wait times out) under the buggy code, so its result is discarded;
    // the assertion that matters is the second prompt below.
    harness.enqueue("go").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    let _ = tokio::time::timeout(
        Duration::from_secs(20),
        harness
            .runtime
            .wait_idle(&harness.session, Duration::from_secs(10)),
    )
    .await;

    let second = tokio::time::timeout(Duration::from_secs(20), async {
        harness.enqueue("again").await;
        harness
            .runtime
            .notify(&harness.session)
            .await
            .expect("notify");
        harness
            .runtime
            .wait_idle(&harness.session, Duration::from_secs(15))
            .await
    })
    .await;
    let second = second.expect("a dead actor must not wedge the session: second turn timed out");
    assert!(
        second.is_ok(),
        "a dead actor must not wedge the session: {:?}",
        second.err()
    );

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    let reply = assistants
        .iter()
        .find(|message| message["finish"] == "stop")
        .unwrap_or_else(|| panic!("no finished assistant reply after respawn: {messages:?}"));
    assert_eq!(reply["content"][0]["type"], "text");
    assert_eq!(reply["content"][0]["text"], "still alive");
    assert!(harness.sink.has("session.execution.succeeded"));
}
