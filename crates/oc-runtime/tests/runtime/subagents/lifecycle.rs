//! Foreground/background subagent lifecycle, timeouts, permission cleanup, and limits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::session_messages;
use crate::support::*;

#[tokio::test]
async fn subagent_foreground_returns_only_the_child_final_text() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "subagent",
                "call_1",
                json!({
                    "agent": "general",
                    "description": "greet",
                    "prompt": "say hi",
                    "background": false,
                }),
            ),
            text_script("child final"),
            text_script("parent done"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1, "one child was created");
    let child = &children[0];
    let messages = session_messages(&harness.store, child).await;
    let visible: Vec<&Value> = messages
        .iter()
        .filter(|message| message["type"] != "idle")
        .collect();
    assert_eq!(visible.len(), 2, "user prompt plus assistant answer");
    assert_eq!(visible[0]["type"], "user");
    assert_eq!(visible[0]["text"], "say hi");
    assert_eq!(visible[1]["type"], "assistant");
    assert_eq!(visible[1]["content"][0]["text"], "child final");

    let requests = harness.model.requests();
    let result = request_tool_result(requests.last().expect("parent request"), 0);
    assert_eq!(
        result,
        format!("<subagent sessionID=\"{child}\" state=\"completed\">child final</subagent>")
    );
    assert!(
        !result.contains("say hi"),
        "the child prompt is not replayed"
    );
    assert!(
        !result.contains("tool"),
        "the child transcript stays private"
    );
}

#[tokio::test]
async fn subagent_foreground_waits_for_a_delayed_child_response() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "subagent",
                "call_delayed",
                json!({
                    "agent": "general",
                    "description": "delayed",
                    "prompt": "think slowly",
                    "background": false,
                }),
            ),
            delayed_text_script("delayed child text", Duration::from_millis(150)),
            text_script("parent done"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 5}}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1);
    let requests = harness.model.requests();
    let result = request_tool_result(requests.last().expect("parent request"), 0);
    assert!(
        result.contains(">delayed child text</subagent>"),
        "empty or early completion: {result}"
    );
    assert!(result.contains("state=\"completed\""));
}

#[tokio::test]
async fn subagent_background_returns_running_and_the_watcher_reports() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "background child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let output = result.output.expect("output");
    let status: Value = serde_json::from_str(&output).expect("json output");
    assert_eq!(status["status"], "running");
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    let deadline = Instant::now() + EVENT_WAIT;
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert!(
                message["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("child final")
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watcher never reported the child"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        session_messages(&harness.store, &child)
            .await
            .iter()
            .filter(|message| message["type"] == "assistant")
            .count(),
        1
    );

    harness
        .runtime
        .wait_idle(&harness.session, IDLE_WAIT)
        .await
        .expect("parent settles after the synthetic message");
    let delivered: Vec<Value> = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .collect();
    assert_eq!(delivered.len(), 1, "completion is delivered exactly once");
    assert_eq!(delivered[0]["metadata"]["state"], "completed");
    assert!(
        delivered[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("state=\"completed\"")
    );
}

#[tokio::test]
async fn subagent_reports_a_deleted_child_as_failed() {
    let harness = Harness::new(
        vec![Script::HangAfter(Vec::new())],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 60}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let context = tool_context(&harness, &harness.session);
    let handle = tokio::spawn(async move {
        tool.execute(
            &context,
            json!({
                "agent": "general",
                "description": "doomed child",
                "prompt": "work",
                "background": false,
            }),
        )
        .await
    });

    let child = tokio::time::timeout(EVENT_WAIT, async {
        loop {
            for id in child_sessions(&harness.store, &harness.session).await {
                if !session_messages(&harness.store, &id).await.is_empty() {
                    return id;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child prompt was promoted");
    harness
        .store
        .delete_session(&child)
        .await
        .expect("child deleted");

    // A deleted child is a FAILURE for the parent: an `Ok(ToolResult)` here
    // would be published as `session.tool.success` even though no result exists.
    let error = tokio::time::timeout(EVENT_WAIT, handle)
        .await
        .expect("wait_child notices the deleted session")
        .expect("tool task")
        .expect_err("a deleted child must not report success");
    assert!(
        error.message.contains("state=\"failed\""),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("session deleted"),
        "{}",
        error.message
    );
    let metadata = error.metadata.as_ref().expect("failure metadata");
    assert_eq!(metadata["sessionID"], child.as_str());
    assert_eq!(metadata["status"], "failed");
}

#[tokio::test]
async fn subagent_background_watcher_reports_a_timeout() {
    let harness = Harness::new(
        vec![delayed_text_script("late child", Duration::from_secs(10))],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": true, "timeout_seconds": 1}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "slow child",
                "prompt": "work",
            }),
        )
        .await
        .expect("background spawn");
    let status: Value =
        serde_json::from_str(result.output.as_deref().expect("output")).expect("json output");
    assert_eq!(status["status"], "running");
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    let message = tokio::time::timeout(EVENT_WAIT, async {
        loop {
            if let Some(message) = session_messages(&harness.store, &harness.session)
                .await
                .into_iter()
                .find(|message| {
                    message["type"] == "synthetic"
                        && message["metadata"]["source"] == "subagent"
                        && message["metadata"]["childID"] == child.as_str()
                })
            {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded watcher reported the timeout");
    let text = message["text"].as_str().unwrap_or_default();
    assert!(text.contains("state=\"failed\""), "{text}");
    assert!(text.contains("reason=\"timeout after 1s\""), "{text}");
    assert_eq!(message["metadata"]["state"], "failed");
    assert_eq!(message["metadata"]["reason"], "timeout after 1s");

    // The timeout cancels the child instead of leaving a zombie behind.
    harness
        .runtime
        .wait_idle(&child, IDLE_WAIT)
        .await
        .expect("timed-out child settles");
    assert!(
        !harness.runtime.active().contains(&child),
        "timed-out child is still active"
    );
    let child_messages = session_messages(&harness.store, &child).await;
    let idle = child_messages
        .iter()
        .find(|message| message["type"] == "idle")
        .expect("child idle row");
    assert_eq!(idle["outcome"], "interrupted");

    // Exactly one synthetic describes this timeout.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let delivered = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .count();
    assert_eq!(delivered, 1, "timeout must be delivered exactly once");
}

#[tokio::test]
async fn subagent_foreground_timeout_cancels_the_child_once() {
    let harness = Harness::new(
        vec![delayed_text_script("late child", Duration::from_secs(10))],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 1}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let started = Instant::now();
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "slow foreground child",
                "prompt": "work",
            }),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "foreground wait must bound to one timeout: {elapsed:?}"
    );
    // A timeout is a FAILURE: the tool must not report a running child as a
    // successful spawn.
    let error = result.expect_err("a timed-out subagent must not report success");
    assert!(
        error.message.contains("timed out after 1s"),
        "{}",
        error.message
    );
    let metadata = error.metadata.as_ref().expect("timeout metadata");
    assert_eq!(metadata["status"], "failed");
    assert_eq!(metadata["reason"], "timeout after 1s");
    let child = metadata["sessionID"]
        .as_str()
        .expect("the error names the timed-out child")
        .to_owned();
    assert!(
        error.message.contains(&child),
        "the model must be able to tell which child timed out: {}",
        error.message
    );

    // The failed synthetic must arrive within one short settle window, not a
    // second full timeout.
    let message = tokio::time::timeout(Duration::from_millis(2_500), async {
        loop {
            if let Some(message) = session_messages(&harness.store, &harness.session)
                .await
                .into_iter()
                .find(|message| {
                    message["type"] == "synthetic"
                        && message["metadata"]["source"] == "subagent"
                        && message["metadata"]["childID"] == child.as_str()
                })
            {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("foreground timeout reported within the settle window");
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
        "timeout report took {:?}",
        started.elapsed()
    );
    let text = message["text"].as_str().unwrap_or_default();
    assert!(text.contains("state=\"failed\""), "{text}");
    assert!(text.contains("reason=\"timeout after 1s\""), "{text}");
    assert_eq!(message["metadata"]["state"], "failed");
    assert_eq!(message["metadata"]["reason"], "timeout after 1s");

    harness
        .runtime
        .wait_idle(&child, IDLE_WAIT)
        .await
        .expect("timed-out child settles");
    assert!(
        !harness.runtime.active().contains(&child),
        "timed-out child is still active"
    );
    let child_messages = session_messages(&harness.store, &child).await;
    let idle = child_messages
        .iter()
        .find(|message| message["type"] == "idle")
        .expect("child idle row");
    assert_eq!(idle["outcome"], "interrupted");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let delivered = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .count();
    assert_eq!(delivered, 1, "timeout must be delivered exactly once");
}

#[tokio::test]
async fn subagent_timeout_is_reported_as_failed_tool_outcome() {
    // Regression: the timeout branch used to return `Ok(ToolResult)` with
    // `{"status":"running"}`, which the turn loop maps to
    // `ToolOutcome::Completed` and publishes `session.tool.success`. A child
    // that was just cancelled must never be reported as a successful spawn.
    let harness = Harness::new(
        vec![
            named_tool_script(
                "subagent",
                "call_timeout",
                json!({
                    "agent": "general",
                    "description": "slow child",
                    "prompt": "work",
                    "background": false,
                }),
            ),
            delayed_text_script("never arrives", Duration::from_secs(10)),
            text_script("parent recovered"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 1}}),
        None,
    )
    .await;
    harness.prompt("spawn a slow child").await;

    // The turn loop recorded a FAILED tool call, not a successful one.
    assert!(
        harness.sink.has("session.tool.failed"),
        "timeout must publish session.tool.failed"
    );
    assert!(
        !harness.sink.has("session.tool.success"),
        "a timed-out subagent must not publish session.tool.success: {:?}",
        harness.sink.kinds()
    );
    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1, "one failed tool call: {failures:?}");
    assert_eq!(failures[0]["error"]["type"], "ToolError");
    let message = failures[0]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("timed out after 1s"), "{message}");
    assert_eq!(failures[0]["executed"], true);

    // The persisted assistant part agrees with the event.
    let messages = session_messages(&harness.store, &harness.session).await;
    let assistant = messages
        .iter()
        .find(|message| message["type"] == "assistant")
        .expect("parent assistant message");
    let tool_part = assistant["content"]
        .as_array()
        .and_then(|parts| parts.iter().find(|part| part["type"] == "tool"))
        .expect("parent tool part");
    assert_eq!(tool_part["state"]["status"], "error");

    // The out-of-band synthetic still reports the timeout exactly once.
    let child = child_sessions(&harness.store, &harness.session)
        .await
        .first()
        .expect("child created")
        .clone();
    let synthetic = tokio::time::timeout(Duration::from_millis(2_500), async {
        loop {
            let messages = session_messages(&harness.store, &harness.session).await;
            if let Some(synthetic) = messages.into_iter().find(|message| {
                message["type"] == "synthetic"
                    && message["metadata"]["source"] == "subagent"
                    && message["metadata"]["childID"] == child.as_str()
            }) {
                return synthetic;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("synthetic timeout report arrived");
    assert_eq!(synthetic["metadata"]["state"], "failed");

    // Defect 4: the durable child row must not claim success.
    let child_row = harness
        .store
        .get_session(&child)
        .await
        .expect("child row")
        .expect("child exists");
    assert_eq!(
        child_row["outcome"], "interrupted",
        "the child session row must record the interruption"
    );
}

#[tokio::test]
async fn subagent_timeout_drops_a_pending_permission_ask() {
    let handler = Arc::new(PendingReadAsk::default());
    let harness = Harness::new(
        vec![tool_script(
            "call_pending_read",
            json!({"path": "hello.txt", "expectation": expectation()}),
        )],
        handler.clone(),
        json!({
            "permission": {"read": {"*": "ask"}},
            "subagents": {"background_default": false, "timeout_seconds": 1},
        }),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let started = Instant::now();
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "asks for permission",
                "prompt": "read the file",
            }),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "foreground wait must bound to one timeout: {:?}",
        started.elapsed()
    );
    let error = result.expect_err("a timed-out subagent must not report success");
    assert!(
        error.message.contains("timed out after 1s"),
        "{}",
        error.message
    );
    let metadata = error.metadata.as_ref().expect("timeout metadata");
    assert_eq!(metadata["status"], "failed");
    let child = metadata["sessionID"]
        .as_str()
        .expect("the error names the timed-out child")
        .to_owned();

    let message = tokio::time::timeout(Duration::from_millis(2_500), async {
        loop {
            if let Some(message) = session_messages(&harness.store, &harness.session)
                .await
                .into_iter()
                .find(|message| {
                    message["type"] == "synthetic"
                        && message["metadata"]["source"] == "subagent"
                        && message["metadata"]["childID"] == child.as_str()
                })
            {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout reported");
    let text = message["text"].as_str().unwrap_or_default();
    assert!(text.contains("reason=\"timeout after 1s\""), "{text}");

    harness
        .runtime
        .wait_idle(&child, IDLE_WAIT)
        .await
        .expect("timed-out child settles");
    assert!(
        !harness.runtime.active().contains(&child),
        "timed-out child is still active"
    );
    // The permission guard was dropped when the child was cancelled.
    assert_eq!(handler.pending(), 0, "the permission ask is still pending");
    assert_eq!(handler.dropped(), 1, "the ask was raised and dropped once");
    let child_messages = session_messages(&harness.store, &child).await;
    let idle = child_messages
        .iter()
        .find(|message| message["type"] == "idle")
        .expect("child idle row");
    assert_eq!(idle["outcome"], "interrupted");

    let delivered = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .count();
    assert_eq!(delivered, 1, "timeout must be delivered exactly once");
}

#[tokio::test]
async fn subagent_rejects_depth_and_child_limits_before_creation() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {
            "max_depth": 5,
            "max_children_per_session": 0,
            "background_default": false,
        }}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let ctx = tool_context(&harness, &harness.session);

    let depth = tool
        .execute(
            &ctx,
            json!({
                "agent": "general",
                "description": "too deep",
                "prompt": "work",
                "spawn": {"max_depth": 0},
            }),
        )
        .await
        .expect_err("depth limit");
    assert!(depth.message.contains("depth"), "{}", depth.message);
    let children = tool
        .execute(
            &ctx,
            json!({"agent": "general", "description": "too many", "prompt": "work"}),
        )
        .await
        .expect_err("child limit");
    assert!(
        children.message.contains("children"),
        "{}",
        children.message
    );
    assert!(
        child_sessions(&harness.store, &harness.session)
            .await
            .is_empty()
    );
}
