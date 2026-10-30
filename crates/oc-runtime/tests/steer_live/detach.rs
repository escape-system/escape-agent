//! Detaching a foreground subagent and background completion delivery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_llm::{ContentPart, LlmRequest};
use oc_store::Store;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::support::*;

struct WaitForDelivery {
    store: Arc<Store>,
    needle: String,
}

#[async_trait::async_trait]
impl Tool for WaitForDelivery {
    fn name(&self) -> &'static str {
        "wait_for_delivery"
    }

    fn description(&self) -> &'static str {
        "Test-only: blocks until a pending inbox row contains the needle."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let items = self
                .store
                .list_inbox(&ctx.session_id)
                .await
                .map_err(|error| ToolError::new(error.to_string()))?;
            let found = items.iter().any(|item| {
                item.pointer("/payload/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(&self.needle))
            });
            if found {
                return Ok(ToolResult::text("delivery pending"));
            }
            if Instant::now() >= deadline {
                return Err(ToolError::new(format!(
                    "timed out waiting for an inbox row containing {:?}",
                    self.needle
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test]
async fn background_subagent_completion_reaches_the_running_turn() {
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![
                    call(
                        "call_spawn",
                        "subagent",
                        json!({
                            "agent": "general",
                            "description": "quick child",
                            "prompt": "child work",
                            "background": true,
                        }),
                    ),
                    call("call_wait", "wait_for_delivery", json!({})),
                ],
            },
            Step::Text {
                text: "child done work".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "state=\"completed\"".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |store| {
            vec![Arc::new(WaitForDelivery {
                store: Arc::clone(store),
                needle: "<subagent".to_owned(),
            })]
        },
    )
    .await;

    harness.prompt("start").await;
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "parent step one, the background child, then parent step two"
    );
    assert!(
        contains_text(&requests[2], "<subagent")
            && contains_text(&requests[2], "state=\"completed\"")
            && contains_text(&requests[2], "child done work"),
        "the step after the spawn carries the completion envelope"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 2, "parent steps only");
    assert!(
        message_text(assistants[1]).contains("mentions state=\"completed\""),
        "the turn's final answer saw the completion: {}",
        message_text(assistants[1])
    );
    let synthetic = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic.len(),
        1,
        "the completion is delivered exactly once"
    );
    assert!(message_text(synthetic[0]).contains("<subagent"));
    let synthetic_id = synthetic[0]["id"].as_str().expect("synthetic id");
    assert_eq!(
        harness.delivery_count(synthetic_id),
        1,
        "one delivery event for the completion"
    );
    assert_eq!(harness.model.calls(), 3);
    assert!(
        harness.inbox().await.is_empty(),
        "the completion was consumed mid-turn, not left for settle"
    );
}

/// Tool results are [`ContentPart::ToolResult`], so [`contains_text`] cannot see
/// them; the detach contract is about what the tool told the model.
fn contains_any_text(request: &LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message.content.iter().any(|part| match part {
            ContentPart::Text { text } => text.contains(needle),
            ContentPart::ToolResult { content, .. } => content.contains(needle),
            _ => false,
        })
    })
}

impl Harness {
    /// Poll the parent transcript for the first synthetic message containing
    /// `needle`, so no assertion depends on scheduling luck.
    async fn wait_for_synthetic(&self, needle: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let messages = self.messages().await;
            let found = of_type(&messages, "synthetic")
                .into_iter()
                .find(|message| message_text(message).contains(needle))
                .cloned();
            if let Some(found) = found {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no synthetic message containing {needle:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test]
async fn background_request_detaches_a_foreground_subagent_wait() {
    let child_gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "call_spawn",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "slow child",
                        "prompt": "child work",
                        "background": false,
                    }),
                )],
            },
            Step::Echo {
                gate: Some(Arc::clone(&child_gate)),
                needle: "child work".to_owned(),
            },
            Step::Text {
                text: "parent continued".to_owned(),
            },
            Step::Text {
                text: "saw child completion".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(harness.next_started().await, 0, "the parent step starts");
    assert_eq!(
        harness.next_started().await,
        1,
        "the child is running while the parent waits"
    );

    let requested = Instant::now();
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("background request"),
        "the parent execution is running"
    );

    // The foreground wait must abandon within one poll tick and let the turn
    // continue, although the child is still gated.
    let next = tokio::time::timeout(Duration::from_secs(1), harness.next_started())
        .await
        .expect("the detach returned the parent's next step within 1s");
    assert_eq!(next, 2, "the parent turn continued to its next step");
    assert!(requested.elapsed() < Duration::from_secs(1));

    let requests = harness.model.requests();
    assert!(
        contains_any_text(&requests[2], "\"status\":\"running\""),
        "the tool result reports a running child: {:?}",
        requests[2].messages
    );

    child_gate.notify_one();
    let synthetic = harness.wait_for_synthetic("<subagent").await;
    harness.idle().await;

    assert!(
        message_text(&synthetic).contains("state=\"completed\"")
            && message_text(&synthetic).contains("mentions child work"),
        "the child finished on its own and delivered exactly once: {}",
        message_text(&synthetic)
    );
    let messages = harness.messages().await;
    let synthetic_messages = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic_messages.len(),
        1,
        "exactly one completion reaches the parent"
    );
    let assistants = of_type(&messages, "assistant");
    assert!(
        assistants
            .iter()
            .any(|message| message_text(message).contains("parent continued")),
        "the detached turn finished without blocking"
    );
    assert!(
        assistants
            .iter()
            .any(|message| message_text(message).contains("saw child completion")),
        "the completion drove a later execution"
    );
}

#[tokio::test]
async fn background_request_on_an_idle_session_returns_false() {
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "spawn_one",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "first child",
                        "prompt": "first work",
                        "background": false,
                    }),
                )],
            },
            Step::Text {
                text: "first child answer".to_owned(),
            },
            Step::Text {
                text: "first parent answer".to_owned(),
            },
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "spawn_two",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "second child",
                        "prompt": "second work",
                        "background": false,
                    }),
                )],
            },
            Step::Text {
                text: "second child answer".to_owned(),
            },
            Step::Text {
                text: "second parent answer".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |_| Vec::new(),
    )
    .await;

    // No actor exists yet.
    assert!(
        !harness
            .runtime
            .request_background(&harness.session)
            .expect("background request"),
        "an unknown session has nothing to detach"
    );

    harness.prompt("first").await;
    harness.idle().await;
    assert!(
        !harness
            .runtime
            .request_background(&harness.session)
            .expect("background request"),
        "an idle session has nothing to detach"
    );

    // The next execution must still wait for a foreground child.
    harness.prompt("second").await;
    harness.idle().await;

    let requests = harness.model.requests();
    assert!(
        contains_any_text(&requests[2], "state=\"completed\"")
            && contains_any_text(&requests[2], "first child answer"),
        "the first foreground wait returned the child's final text"
    );
    assert!(
        contains_any_text(&requests[5], "state=\"completed\"")
            && contains_any_text(&requests[5], "second child answer"),
        "the idle request did not corrupt the next execution"
    );
    let messages = harness.messages().await;
    assert!(
        of_type(&messages, "synthetic").is_empty(),
        "foreground completions are tool results, not synthetic messages"
    );
}

#[tokio::test]
async fn explicit_background_spawn_ignores_a_background_request() {
    let child_gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![
                    call(
                        "call_spawn",
                        "subagent",
                        json!({
                            "agent": "general",
                            "description": "background child",
                            "prompt": "child work",
                            "background": true,
                        }),
                    ),
                    call("call_wait", "wait_for_delivery", json!({})),
                ],
            },
            Step::Echo {
                gate: Some(Arc::clone(&child_gate)),
                needle: "child work".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "state=\"completed\"".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |store| {
            vec![Arc::new(WaitForDelivery {
                store: Arc::clone(store),
                needle: "<subagent".to_owned(),
            })]
        },
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(harness.next_started().await, 0);
    assert_eq!(harness.next_started().await, 1, "the child is running");

    // The parent's step is blocked in `wait_for_delivery`, not in a subagent
    // wait; the request must not add a second watcher.
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("background request")
    );

    child_gate.notify_one();
    harness.idle().await;

    let messages = harness.messages().await;
    let synthetic = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic.len(),
        1,
        "the explicit background spawn still delivers exactly once"
    );
    assert!(message_text(synthetic[0]).contains("state=\"completed\""));
    let requests = harness.model.requests();
    assert!(
        contains_any_text(&requests[2], "\"status\":\"running\""),
        "the background tool result is unchanged"
    );
}

#[tokio::test]
async fn a_second_background_request_after_consumption_has_no_effect() {
    let child_gate = Arc::new(Notify::new());
    let parent_gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "call_spawn",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "slow child",
                        "prompt": "child work",
                        "background": false,
                    }),
                )],
            },
            Step::Echo {
                gate: Some(Arc::clone(&child_gate)),
                needle: "child work".to_owned(),
            },
            Step::Echo {
                gate: Some(Arc::clone(&parent_gate)),
                needle: "PARENT_SECOND_STEP".to_owned(),
            },
            Step::Text {
                text: "saw child completion".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(harness.next_started().await, 0);
    assert_eq!(harness.next_started().await, 1);
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("background request")
    );

    // The parent is still running while its second step is gated.
    let next = tokio::time::timeout(Duration::from_secs(1), harness.next_started())
        .await
        .expect("the detach returned the parent's next step within 1s");
    assert_eq!(next, 2);
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("second background request"),
        "the parent execution is still running"
    );

    parent_gate.notify_one();
    harness.idle().await;
    child_gate.notify_one();
    harness.wait_for_synthetic("<subagent").await;
    harness.idle().await;

    let messages = harness.messages().await;
    let synthetic = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic.len(),
        1,
        "the second request did not spawn another watcher"
    );
    assert!(message_text(synthetic[0]).contains("state=\"completed\""));
    assert_eq!(
        harness.model.calls(),
        4,
        "spawn, child, parent step two, completion turn"
    );
}
