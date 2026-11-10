//! Subagent lifecycle: foreground/background spawns, limits, talk, digest and schemas.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_runtime::SubagentTools;
use oc_store::{
    Delivery, MessageQuery, NewInbox, NewMessage, NewSession, NewSessionTree, Order, Store,
};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::support::*;

async fn session_messages(store: &Store, session: &str) -> Vec<Value> {
    let mut cursor = None;
    let mut out = Vec::new();
    loop {
        let page = store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 200,
                order: Order::Asc,
                cursor,
                kind: None,
            })
            .await
            .expect("messages");
        out.extend(page.items);
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}
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

    let deadline = Instant::now() + Duration::from_secs(5);
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
        .wait_idle(&harness.session, Duration::from_secs(5))
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

    let child = tokio::time::timeout(Duration::from_secs(5), async {
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
    let error = tokio::time::timeout(Duration::from_secs(5), handle)
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

    let message = tokio::time::timeout(Duration::from_secs(10), async {
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
        .wait_idle(&child, Duration::from_secs(5))
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
        .wait_idle(&child, Duration::from_secs(5))
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
        .wait_idle(&child, Duration::from_secs(5))
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

#[tokio::test]
async fn talk_allows_siblings_parent_and_child_but_rejects_strangers() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"siblings": {"enabled": true}}}),
        None,
    )
    .await;
    let a = create_child(&harness, &harness.session, "ses_child_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_child_b", "general").await;
    create_unrelated_root(&harness, "ses_other_root").await;
    let stranger = create_child(&harness, "ses_other_root", "ses_stranger", "general").await;
    let tool = subagent_tool(&harness, "talk");

    tool.execute(
        &tool_context(&harness, &a),
        json!({"to": b, "kind": "note", "text": "hello sibling"}),
    )
    .await
    .expect("siblings may talk");
    let inbox = harness.store.list_inbox(&b).await.expect("inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["type"], "peer");
    assert_eq!(inbox[0]["payload"]["from"], a.as_str());
    assert_eq!(inbox[0]["payload"]["metadata"]["source"], "peer");

    tool.execute(
        &tool_context(&harness, &a),
        json!({"to": harness.session, "kind": "answer", "text": "report"}),
    )
    .await
    .expect("children may answer the parent");
    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({"to": b, "kind": "question", "text": "status?"}),
    )
    .await
    .expect("parents may talk to children");

    let denied = tool
        .execute(
            &tool_context(&harness, &a),
            json!({"to": stranger, "kind": "note", "text": "hi"}),
        )
        .await
        .expect_err("strangers are denied");
    assert!(
        denied.message.contains("not siblings"),
        "{}",
        denied.message
    );
}

#[tokio::test]
async fn roster_omits_parent_for_a_root_session() {
    // Regression: a root caller used to serialize `"parent": null`, which
    // violates AGENTS.md ("Optional JSON fields are omitted, never null") and
    // the documented object shape in `docs/05-subagents.md`.
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let child = create_child(
        &harness,
        &harness.session,
        "ses_roster_root_child",
        "general",
    )
    .await;
    let tool = subagent_tool(&harness, "roster");
    let result = tool
        .execute(&tool_context(&harness, &harness.session), json!({}))
        .await
        .expect("roster");
    let body: Value = serde_json::from_str(&result.output.expect("output")).expect("json");
    let object = body.as_object().expect("roster body is an object");
    assert!(
        object.get("parent").is_none(),
        "a root caller must omit `parent`, got {:?}",
        object.get("parent")
    );
    assert!(
        !body.to_string().contains("\"parent\""),
        "the serialized body must not mention parent at all: {body}"
    );
    assert!(object.get("siblings").is_some(), "siblings stays present");
    assert!(object.get("children").is_some(), "children stays present");
    assert!(body["siblings"].as_array().expect("siblings").is_empty());
    assert_eq!(body["children"].as_array().expect("children").len(), 1);
    assert_eq!(body["children"][0]["id"], child.as_str());
}

#[tokio::test]
async fn roster_returns_ids_agents_statuses_and_titles_only() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let a = create_child(&harness, &harness.session, "ses_roster_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_roster_b", "general").await;
    let tool = subagent_tool(&harness, "roster");
    let result = tool
        .execute(&tool_context(&harness, &a), json!({}))
        .await
        .expect("roster");
    let body: Value = serde_json::from_str(&result.output.expect("output")).expect("json");
    assert_eq!(body["parent"]["id"], harness.session.as_str());
    assert_eq!(body["siblings"].as_array().expect("siblings").len(), 1);
    assert_eq!(body["siblings"][0]["id"], b.as_str());
    assert!(body["children"].as_array().expect("children").is_empty());
    for key in ["id", "agent", "status", "title"] {
        assert!(body["parent"].get(key).is_some(), "missing {key}");
    }
    for field in ["content", "messages", "transcript"] {
        assert!(body["parent"].get(field).is_none(), "leaked {field}");
    }
}

#[tokio::test]
async fn session_write_follows_descendant_ancestry_only() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let a = create_child(&harness, &harness.session, "ses_write_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_write_b", "general").await;
    let tool = subagent_tool(&harness, "session_write");

    let sibling = tool
        .execute(
            &tool_context(&harness, &a),
            json!({"session_id": b, "text": "steer"}),
        )
        .await
        .expect_err("siblings are not writable");
    assert!(
        sibling.message.contains("descendant"),
        "{}",
        sibling.message
    );
    let upward = tool
        .execute(
            &tool_context(&harness, &b),
            json!({"session_id": harness.session, "text": "steer"}),
        )
        .await
        .expect_err("parents are not writable");
    assert!(upward.message.contains("descendant"), "{}", upward.message);

    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({"session_id": b, "text": "from parent", "delivery": "queue"}),
    )
    .await
    .expect("parents write descendants");
    let inbox = harness.store.list_inbox(&b).await.expect("inbox");
    assert!(
        inbox.iter().any(|item| {
            item["type"] == "synthetic"
                && item["payload"]["metadata"]["source"] == "peer"
                && item["payload"]["metadata"]["from"] == harness.session.as_str()
        }),
        "{inbox:?}"
    );
    tool.execute(
        &tool_context(&harness, &b),
        json!({"session_id": b, "text": "self note"}),
    )
    .await
    .expect("self notes are allowed");
}

#[tokio::test]
async fn digest_summarizes_with_a_scribe_caches_and_requires_one() {
    let scribe = FakeScribe::new("focused summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let tool = subagent_tool(&harness, "digest");
    // A tiny budget truncates the raw answer (1 token == 4 chars)...
    let first = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": harness.session, "question": "What happened?", "max_tokens": 1}),
        )
        .await
        .expect("digest");
    assert_eq!(first.output.as_deref(), Some("focu"));
    assert_eq!(first.metadata.expect("metadata")["cached"], false);
    // ...and a cache hit must still honor the caller's own budget instead of
    // replaying the first caller's truncation.
    let second = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": harness.session, "question": "What happened?", "max_tokens": 100}),
        )
        .await
        .expect("cached digest");
    assert_eq!(second.output.as_deref(), Some("focused summary"));
    assert_eq!(second.metadata.expect("metadata")["cached"], true);
    assert_eq!(scribe.prompts.lock().expect("prompts").len(), 1);
    assert!(scribe.prompts.lock().expect("prompts")[0].contains("What happened?"));

    let unsupported = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let error = subagent_tool(&unsupported, "digest")
        .execute(&tool_context(&unsupported, &unsupported.session), json!({}))
        .await
        .expect_err("digest without a scribe");
    assert!(
        error.message.contains("DigestUnavailableError"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn digest_includes_the_complete_final_answer() {
    let scribe = FakeScribe::new("summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let child = create_child(&harness, &harness.session, "ses_digest_final", "general").await;
    for index in 0..60 {
        append_runtime_message(
            &harness,
            &child,
            &format!("msg_digest_ctx_{index}"),
            "user",
            json!({"text": format!("earlier message {index}")}),
        )
        .await;
    }
    append_runtime_message(
        &harness,
        &child,
        "msg_digest_tool",
        "assistant",
        json!({"content": [
            {"type": "text", "text": "checking the build"},
            {
                "type": "tool",
                "id": "call_digest",
                "name": "read",
                "state": {"status": "completed", "input": {}, "content": [], "metadata": {}},
                "time": {"created": 1}
            }
        ]}),
    )
    .await;
    let final_text = format!("BEGIN-REPORT {} END-REPORT", "z".repeat(3_976));
    assert_eq!(final_text.chars().count(), 4_000);
    append_runtime_message(
        &harness,
        &child,
        "msg_digest_final",
        "assistant",
        json!({"content": [{"type": "text", "text": final_text}]}),
    )
    .await;

    subagent_tool(&harness, "digest")
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child, "question": "What did the child report?"}),
        )
        .await
        .expect("digest");

    let prompt = scribe.prompts.lock().expect("prompts")[0].clone();
    assert!(prompt.contains("FINAL ANSWER (complete):"), "{prompt}");
    assert!(prompt.contains(&final_text), "final answer was truncated");
    assert!(!prompt.contains("final answer truncated"), "{prompt}");
    assert!(prompt.contains("RECENT CONTEXT (excerpted):"), "{prompt}");
    assert!(prompt.contains("tool read: completed"), "{prompt}");
    let answer_at = prompt.find("FINAL ANSWER (complete):").expect("marker");
    let context_at = prompt.find("RECENT CONTEXT (excerpted):").expect("marker");
    assert!(answer_at < context_at, "final answer comes first");
}

#[tokio::test]
async fn digest_truncates_an_oversized_final_answer_at_the_budget() {
    let scribe = FakeScribe::new("summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let child = create_child(&harness, &harness.session, "ses_digest_oversize", "general").await;
    let final_text = format!("{}TAIL-MARKER", "y".repeat(6_500));
    append_runtime_message(
        &harness,
        &child,
        "msg_digest_oversize_final",
        "assistant",
        json!({"content": [{"type": "text", "text": final_text}]}),
    )
    .await;

    subagent_tool(&harness, "digest")
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child}),
        )
        .await
        .expect("digest");

    let prompt = scribe.prompts.lock().expect("prompts")[0].clone();
    assert!(prompt.contains(&"y".repeat(6_000)), "budget prefix missing");
    assert!(
        prompt.contains("…[final answer truncated at 6000 chars]"),
        "{prompt}"
    );
    assert!(!prompt.contains("TAIL-MARKER"), "exceeded the budget");
}

async fn append_runtime_message(
    harness: &Harness,
    session: &str,
    id: &str,
    kind: &str,
    data: Value,
) {
    harness
        .store
        .append_message(NewMessage {
            id: id.to_owned(),
            session_id: session.to_owned(),
            kind: kind.to_owned(),
            data,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("message appended");
}

#[tokio::test]
async fn digest_is_limited_to_the_callers_subtree() {
    let scribe = FakeScribe::new("scoped summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let child_a = create_child(&harness, &harness.session, "ses_digest_a", "general").await;
    let child_b = create_child(&harness, &harness.session, "ses_digest_b", "general").await;
    create_unrelated_root(&harness, "ses_digest_other").await;
    let tool = subagent_tool(&harness, "digest");

    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({"session_id": child_a, "question": "What did the child do?"}),
    )
    .await
    .expect("a parent digests its descendant");

    tool.execute(
        &tool_context(&harness, &child_a),
        json!({"session_id": child_a}),
    )
    .await
    .expect("a session digests itself");

    for (caller, target) in [
        (child_a.as_str(), harness.session.as_str()),
        (child_a.as_str(), child_b.as_str()),
        (child_a.as_str(), "ses_digest_other"),
        (harness.session.as_str(), "ses_digest_other"),
    ] {
        let error = tool
            .execute(
                &tool_context(&harness, caller),
                json!({"session_id": target}),
            )
            .await
            .expect_err("outside the caller's subtree");
        assert_eq!(
            error.message,
            format!("DigestScopeError: session {target} is outside your subtree")
        );
    }
}

#[tokio::test]
async fn subagent_model_override_resolves_and_unavailable_models_fail_before_spawn() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {
            "background_default": true,
            "default_model": "test/alpha",
            "agents": {"general": {"model": "test/alpha"}},
        }}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({
            "agent": "general",
            "description": "modelled",
            "prompt": "work",
            "model": "test/alpha",
        }),
    )
    .await
    .expect("spawn with an available model");
    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1);
    let session = harness
        .store
        .get_session(&children[0])
        .await
        .expect("session")
        .expect("exists");
    assert_eq!(session["model"]["providerID"], "test");
    assert_eq!(session["model"]["id"], "alpha");

    let error = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "missing",
                "prompt": "work",
                "model": "test/missing",
            }),
        )
        .await
        .expect_err("unavailable model");
    assert!(error.message.contains("unavailable"), "{}", error.message);
    assert!(error.message.contains("test/missing"), "{}", error.message);
    assert_eq!(
        child_sessions(&harness.store, &harness.session).await.len(),
        1,
        "no session is created for an unavailable model"
    );
}

#[tokio::test]
async fn parent_completion_message_carries_the_subagent_metadata() {
    let harness = Harness::new(
        vec![text_script("finished work")],
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
                "description": "labelled child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["agent"], "general");
            assert_eq!(message["metadata"]["state"], "completed");
            let text = message["text"].as_str().expect("envelope");
            assert!(
                text.starts_with(&format!(
                    "<subagent sessionID=\"{child}\" state=\"completed\" description=\"labelled child\">"
                )),
                "{text}"
            );
            assert!(text.contains("finished work"));
            break;
        }
        assert!(Instant::now() < deadline, "no completion message");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn subagent_step_limit_is_reported_to_the_parent() {
    // The child agent gets a budget of one tool-enabled step, so its scripted
    // read exhausts the budget and the next (final, tool-less) call only
    // produces a summary. Without the qualifier the parent would see an
    // ordinary `state="completed"` result and plan on a truncated answer.
    let harness = Harness::new(
        vec![
            tool_script(
                "read_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("partial summary"),
        ],
        Arc::new(AllowAll),
        json!({
            "agents": {"general": {"steps": 1}},
            "subagents": {"background_default": false},
        }),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "truncated child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["state"], "completed");
            assert_eq!(message["metadata"]["stepLimit"], true);
            let steps = message["metadata"]["steps"]
                .as_u64()
                .expect("steps recorded on the envelope metadata");
            assert_eq!(
                steps, 1,
                "the reported count is the configured budget, not an arbitrary number"
            );
            let text = message["text"].as_str().expect("envelope");
            assert!(
                text.contains(&format!(
                    "\n[step budget exhausted after {steps} steps; result may be incomplete]"
                )),
                "{text}"
            );
            assert!(text.contains("partial summary"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "no step-limit report");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn subagent_that_stops_on_its_own_has_no_step_limit_marker() {
    // Negative control for the qualifier: the child answers in one tool-less
    // step, so its turn never hits the budget. The parent envelope must stay
    // byte-identical to the ordinary completion (no `stepLimit`, no `steps`,
    // no appended caveat) and no `session.execution.step_limit` event may be
    // committed for the child.
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
                "description": "plain child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["state"], "completed");
            assert!(
                message["metadata"].get("stepLimit").is_none(),
                "an unexhausted budget leaves no marker: {}",
                message["metadata"]
            );
            assert!(
                message["metadata"].get("steps").is_none(),
                "an unexhausted budget records no step count: {}",
                message["metadata"]
            );
            let text = message["text"].as_str().expect("envelope");
            assert!(text.contains("child final"), "{text}");
            assert!(!text.contains("step budget exhausted"), "{text}");
            break;
        }
        assert!(Instant::now() < deadline, "no completion message");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let logged = harness
        .store
        .events_after(&child, 0, 1_000)
        .await
        .expect("event log");
    assert!(
        !logged
            .iter()
            .any(|event| event.kind == "session.execution.step_limit"),
        "the child's own log carries no qualifier: {:?}",
        logged.iter().map(|event| &event.kind).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn subagent_schemas_match_the_documented_keys() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let registry = SubagentTools::new(harness.runtime.clone()).registry();
    let keys = |name: &str| -> Vec<String> {
        let mut keys: Vec<String> = registry.get(name).expect("tool").schema()["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys("subagent"),
        [
            "agent",
            "background",
            "description",
            "group",
            "lane",
            "model",
            "prompt",
            "role",
            "sessionID",
            "spawn"
        ]
    );
    assert_eq!(keys("talk"), ["kind", "request_reply", "text", "to"]);
    assert!(keys("roster").is_empty());
    assert_eq!(
        keys("session_write"),
        ["delivery", "interrupt", "session_id", "text"]
    );
    assert_eq!(
        keys("digest"),
        ["max_tokens", "question", "session_id", "tool_call_id"]
    );
    let subagent = registry.get("subagent").expect("subagent").schema();
    assert_eq!(subagent["required"], json!(["description", "prompt"]));
    let agent = subagent["properties"]["agent"]
        .as_object()
        .expect("agent schema");
    let agent_description = agent["description"].as_str().expect("agent description");
    assert!(
        agent_description.contains("general"),
        "spawnable agents are named: {agent_description}"
    );
    assert!(
        subagent["properties"]["description"]["type"] == "string"
            && subagent["properties"]["prompt"]["type"] == "string"
    );
    assert_eq!(subagent["additionalProperties"], false);
    assert_eq!(
        subagent["properties"]["role"]["enum"],
        json!(["worker", "coordinator"])
    );
}

#[tokio::test]
async fn subagent_tool_is_filtered_out_for_agents_that_cannot_spawn() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let session = "ses_explore_root".to_owned();
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: session.clone(),
                parent_id: None,
                project_id: "prj_runtime_test".to_owned(),
                directory: path(&harness.work),
                title: Some("explore root".to_owned()),
                agent: Some("explore".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: session.clone(),
                depth: 0,
                role: "primary".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("explore session");
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "hi", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness.runtime.notify(&session).await.expect("notify");
    harness
        .runtime
        .wait_idle(&session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        !names.contains(&"subagent"),
        "the explore agent denies `subagent`: {names:?}"
    );
    assert!(
        names.contains(&"read"),
        "unrelated tools still reach the explore agent: {names:?}"
    );
}

#[tokio::test]
async fn subagent_spawn_records_group_in_metadata() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "description": "grouped worker",
                "prompt": "do work",
                "background": true,
                "group": "wave-alpha",
                "lane": "cargo"
            }),
        )
        .await
        .expect("spawn grouped subagent");
    let output = result.output.expect("output");
    assert!(output.contains("\"status\":\"running\""));

    let children = harness
        .store
        .children(&harness.session)
        .await
        .expect("children");
    assert_eq!(children.len(), 1);
    let child_id = &children[0].id;
    let session = harness
        .store
        .get_session(child_id)
        .await
        .expect("get session")
        .expect("session exists");
    let metadata = session.get("metadata").expect("metadata exists");
    assert_eq!(metadata["group"], "wave-alpha");
    assert_eq!(metadata["lane"], "cargo");
}
