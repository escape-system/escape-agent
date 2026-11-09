use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use oc_llm::LlmEvent;
use serde_json::{Value, json};

use crate::support::{
    Harness, Script, assistants, call, expectation, text_script, tool_script, tool_states,
    wait_for_permission, wait_until_active,
};

#[tokio::test]
async fn text_turn_completes_and_log_records_success() {
    let harness = Harness::new(vec![text_script("Hello from the runtime")], None).await;

    let (status, _) = harness.prompt("hi").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 1);
    assert!(
        assistants[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.execution.succeeded")
    );
    assert_eq!(harness.model.calls(), 1);

    let (status, body) = call(&harness.app, Method::GET, "/api/session/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({ "data": {} }));

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn tool_turn_reads_a_real_file() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                "read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        None,
    )
    .await;

    let (status, _) = harness.prompt("read the file").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    let tool_parts: Vec<&Value> = assistants.iter().flat_map(|m| tool_states(m)).collect();
    assert_eq!(tool_parts.len(), 1);
    assert_eq!(tool_parts[0]["status"], "completed");
    assert_eq!(tool_parts[0]["input"]["path"], "hello.txt");

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.tool.success")
    );
    assert_eq!(harness.model.calls(), 2);

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn permission_ask_waits_for_once_reply() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_shell",
                "shell",
                json!({"command": "echo ok", "expectation": expectation()}),
            ),
            text_script("done"),
        ],
        Some(json!([{"action": "shell", "resource": "*", "effect": "ask"}])),
    )
    .await;

    let (status, _) = harness.prompt("run it").await;
    assert_eq!(status, StatusCode::OK);

    let request = wait_for_permission(&harness).await;
    assert_eq!(request["action"], "shell");
    assert_eq!(request["sessionID"], harness.session);
    assert_eq!(request["source"]["type"], "tool");
    assert!(request["id"].as_str().unwrap().starts_with("per_"));

    let id = request["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.reply(&id, "once").await, StatusCode::NO_CONTENT);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let tool_parts: Vec<&Value> = assistants(&messages)
        .iter()
        .flat_map(|message| tool_states(message))
        .collect();
    assert_eq!(tool_parts.len(), 1);
    assert_eq!(tool_parts[0]["status"], "completed");

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.execution.succeeded")
    );
    assert!(harness.pending().await.is_empty());

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn permission_reject_fails_the_tool() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_shell",
                "shell",
                json!({"command": "echo ok", "expectation": expectation()}),
            ),
            text_script("the tool was rejected"),
        ],
        Some(json!([{"action": "shell", "resource": "*", "effect": "ask"}])),
    )
    .await;

    let (status, _) = harness.prompt("run it").await;
    assert_eq!(status, StatusCode::OK);
    let request = wait_for_permission(&harness).await;
    let id = request["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.reply(&id, "reject").await, StatusCode::NO_CONTENT);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let tool_parts: Vec<&Value> = assistants(&messages)
        .iter()
        .flat_map(|message| tool_states(message))
        .collect();
    assert_eq!(tool_parts.len(), 1);
    assert_eq!(tool_parts[0]["status"], "error");
    assert_eq!(tool_parts[0]["error"]["message"], "not now");

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.tool.failed")
    );

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn interrupt_cancels_a_hanging_stream() {
    let harness = Harness::new(vec![Script::HangAfter(vec![LlmEvent::StepStart])], None).await;

    let (status, _) = harness.prompt("hang").await;
    assert_eq!(status, StatusCode::OK);
    wait_until_active(&harness).await;
    // Interrupting before the step emits its first event leaves no assistant
    // row to mark as aborted; wait for the hanging call to have started.
    let deadline = Instant::now() + Duration::from_secs(5);
    while assistants(&harness.messages().await).is_empty() {
        assert!(
            Instant::now() < deadline,
            "hanging step never created an assistant message"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/interrupt", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({ "interrupted": true }));

    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants[0]["error"]["type"], "MessageAbortedError");
    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.execution.interrupted")
    );

    harness.runtime.expect("runtime").shutdown().await;
}

/// A user stop is terminal: the session does not start a fresh execution, it
/// stays out of `GET /api/session/active`, and no message row is appended by a
/// resume attempt. The durable `idle_reason` stays queryable so the observer
/// (which this harness does not wire) can see the stop.
#[tokio::test]
async fn a_user_stop_does_not_restart_the_session() {
    let harness = Harness::new(vec![Script::HangAfter(vec![LlmEvent::StepStart])], None).await;

    let (status, _) = harness.prompt("hang").await;
    assert_eq!(status, StatusCode::OK);
    wait_until_active(&harness).await;
    // The interrupt route only marks an existing assistant row as aborted; wait
    // for the hanging step to have created it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while assistants(&harness.messages().await).is_empty() {
        assert!(
            Instant::now() < deadline,
            "hanging step never created an assistant message"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let before = harness.messages().await;
    let starts_before = execution_started_events(&harness).await;

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/interrupt", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({ "interrupted": true }));

    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    // Give a would-be restart an ample, deterministic window to appear. Any
    // resume path needs an observer tick and a provider call; both are absent
    // here, so the counts below must be stable.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let starts_after = execution_started_events(&harness).await;
    assert_eq!(
        starts_after, starts_before,
        "a user stop must not start another execution"
    );
    let (_, active) = call(&harness.app, Method::GET, "/api/session/active", None).await;
    assert_eq!(
        active.expect("active body")["data"],
        json!({}),
        "a stopped session must not stay active"
    );

    let after = harness.messages().await;
    let after_types: Vec<String> = after
        .iter()
        .map(|message| {
            format!(
                "{}:{}",
                message["type"].as_str().unwrap_or("?"),
                message["id"].as_str().unwrap_or("?")
            )
        })
        .collect();
    let before_ids: Vec<&str> = before
        .iter()
        .filter_map(|message| message["id"].as_str())
        .collect();
    let appended: Vec<&Value> = after
        .iter()
        .filter(|message| match message["id"].as_str() {
            Some(id) => !before_ids.contains(&id),
            None => true,
        })
        .collect();
    // The only row a stop may add is the terminal `idle` marker. Anything else
    // (an observer nudge or a resumed turn) is the bug this test guards.
    assert!(
        appended.iter().all(|message| message["type"] == "idle"),
        "a stop may only append the idle row; appended = {after_types:?}"
    );
    assert!(
        !after
            .iter()
            .any(|message| matches!(message["type"].as_str(), Some("synthetic" | "observer"))),
        "a user stop must not append a synthetic or observer row; after = {after_types:?}"
    );
    assert_eq!(
        assistants(&after).len(),
        1,
        "exactly the aborted assistant row must remain"
    );
    assert_eq!(
        assistants(&after)[0]["error"]["type"],
        "MessageAbortedError"
    );

    // The observer is not wired into this harness, so assert the observable
    // fact its judge reads: the durable stop reason (`SessionNodeRow`).
    let node = harness
        .store
        .session_node(&harness.session)
        .await
        .expect("session node")
        .expect("session row");
    assert_eq!(node.idle_reason.as_deref(), Some("user"));

    harness.runtime.expect("runtime").shutdown().await;
}

async fn execution_started_events(harness: &Harness) -> usize {
    harness
        .log()
        .await
        .iter()
        .filter(|event| event["type"] == "session.execution.started")
        .count()
}
