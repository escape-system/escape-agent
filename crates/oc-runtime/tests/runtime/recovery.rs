//! Crash recovery for background children.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_store::{NewEvent, NewMessage, NewSession, NewSessionTree};
use serde_json::{Value, json};

use crate::support::*;

/// Counts the durable terminal execution events stored for `session`.
async fn terminals(store: &oc_store::Store, session: &str) -> Vec<String> {
    let mut after = 0;
    let mut kinds = Vec::new();
    loop {
        let events = store
            .events_after(session, after, 1_000)
            .await
            .expect("events");
        let Some(last) = events.last() else {
            return kinds;
        };
        let seq = last.seq;
        kinds.extend(events.into_iter().map(|event| event.kind).filter(|kind| {
            matches!(
                kind.as_str(),
                "session.execution.succeeded"
                    | "session.execution.failed"
                    | "session.execution.interrupted"
            )
        }));
        after = seq;
    }
}

#[tokio::test]
async fn recovery_does_not_add_a_terminal_to_an_already_terminal_session() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_event(NewEvent {
            id: "evt_recovery_succeeded".to_owned(),
            aggregate_id: harness.session.clone(),
            kind: "session.execution.succeeded".to_owned(),
            data: json!({ "sessionID": harness.session }),
            durable: true,
            version: 1,
            created: now,
        })
        .await
        .expect("terminal event");
    // The crashed process left a step without its completion stamp, so
    // recovery has to repair the row.
    harness
        .store
        .append_message(NewMessage {
            id: "msg_unfinished".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "engineer",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{"type": "text", "text": "half an answer"}]
            }),
            time_created: now,
        })
        .await
        .expect("unfinished assistant");
    // Control: a second session with no prior terminal must keep the normal
    // `interrupted` recovery terminal.
    create_unrelated_root(&harness, "ses_never_terminal").await;
    harness
        .store
        .append_message(NewMessage {
            id: "msg_never_finished".to_owned(),
            session_id: "ses_never_terminal".to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "engineer",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{"type": "text", "text": "unfinished"}]
            }),
            time_created: now,
        })
        .await
        .expect("control assistant");

    let runtime = harness.restart();
    runtime.start().await.expect("start");

    let repaired = harness
        .store
        .get_message(&harness.session, "msg_unfinished")
        .await
        .expect("read repaired")
        .expect("repaired row");
    assert!(repaired["time"]["completed"].is_i64());
    assert_eq!(repaired["finish"], "error");

    let terminal = terminals(&harness.store, &harness.session).await;
    assert_eq!(
        terminal,
        vec!["session.execution.succeeded".to_owned()],
        "recovery must not add a terminal to an already-terminal session"
    );
    assert_eq!(
        terminals(&harness.store, "ses_never_terminal").await,
        vec!["session.execution.interrupted".to_owned()],
        "a session with no terminal is still interrupted exactly once"
    );
}

#[tokio::test]
async fn recovery_reports_a_settled_childs_real_outcome() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let child = "ses_settled_child";
    create_child(&harness, &harness.session, child, "general").await;
    let now = oc_types::now_millis();
    harness
        .store
        .mark_session_idle(child, now, "succeeded")
        .await
        .expect("child settled");
    harness
        .store
        .append_message(NewMessage {
            id: "msg_child_final".to_owned(),
            session_id: child.to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now, "streamed": now, "completed": now},
                "agent": "general",
                "model": {"providerID": "test", "id": "alpha"},
                "finish": "stop",
                "content": [{"type": "text", "text": "child final answer"}]
            }),
            time_created: now,
        })
        .await
        .expect("child answer");
    // The parent's in-flight subagent call points at the settled child.
    harness
        .store
        .append_message(NewMessage {
            id: "msg_parent_running".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "engineer",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_subagent",
                    "name": "subagent",
                    "state": {
                        "status": "running",
                        "input": {"description": "child task", "prompt": "do the work"},
                        "metadata": {"sessionID": child, "status": "running"}
                    },
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("parent message");

    let runtime = harness.restart();
    runtime.start().await.expect("start");

    let parent = harness
        .store
        .get_message(&harness.session, "msg_parent_running")
        .await
        .expect("read parent")
        .expect("parent row");
    let state = &parent["content"][0]["state"];
    assert_ne!(state["status"], "error", "{state:?}");
    assert_ne!(
        state["error"]["type"], "ToolAbortedError",
        "a child that succeeded must not be reported as cancelled by the restart: {state:?}"
    );
    let text = state["content"][0]["text"]
        .as_str()
        .expect("completed content text");
    assert!(
        text.contains(&format!("sessionID=\"{child}\" state=\"completed\"")),
        "{text}"
    );
    assert!(text.contains("child final answer"), "{text}");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn crash_recovery_settles_an_interrupted_background_child_once() {
    let harness = Harness::new(
        vec![text_script("recovered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let child = "ses_recovered_child";
    let now = oc_types::now_millis();
    let parent_info = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("parent")
        .expect("parent exists");
    let directory = parent_info["location"]["directory"]
        .as_str()
        .expect("directory")
        .to_owned();
    let project = parent_info["projectID"]
        .as_str()
        .expect("project")
        .to_owned();
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: child.to_owned(),
                parent_id: Some(harness.session.clone()),
                project_id: project,
                directory,
                title: Some("background child".to_owned()),
                agent: Some("general".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: now,
            },
            NewSessionTree {
                root_id: harness.session.clone(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: false,
            },
        )
        .await
        .expect("child session");

    // The parent finished its background spawn: the tool part is complete and
    // carries `status:"running"`, which is what a crashed watcher leaves behind.
    harness
        .store
        .append_message(NewMessage {
            id: "msg_parent_bg".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now, "streamed": now, "completed": now},
                "agent": "engineer",
                "model": {"providerID": "test", "id": "alpha"},
                "finish": "stop",
                "content": [{
                    "type": "tool",
                    "id": "call_bg",
                    "name": "subagent",
                    "executed": true,
                    "state": {
                        "status": "completed",
                        "input": {
                            "description": "background child",
                            "agent": "general",
                            "prompt": "do the work",
                            "background": true
                        },
                        "content": [{
                            "type": "text",
                            "text": format!("{{\"sessionID\":\"{child}\",\"status\":\"running\"}}")
                        }],
                        "metadata": {"sessionID": child, "status": "running"}
                    },
                    "time": {"created": now, "ran": now, "completed": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("parent message");
    // The child died mid-tool.
    harness
        .store
        .append_message(NewMessage {
            id: "msg_child_stale".to_owned(),
            session_id: child.to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "general",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_child",
                    "name": "shell",
                    "state": {"status": "running", "input": {"command": "sleep 30"}},
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("child message");

    let runtime = harness.restart();
    runtime.start().await.expect("start after crash");
    let deadline = Instant::now() + EVENT_WAIT;
    let synthetics = loop {
        let messages = harness.messages().await;
        let synthetics: Vec<Value> = messages
            .iter()
            .filter(|message| message["type"] == "synthetic")
            .cloned()
            .collect();
        if !synthetics.is_empty() || Instant::now() > deadline {
            break synthetics;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(synthetics.len(), 1, "{synthetics:?}");
    let synthetic = &synthetics[0];
    assert_eq!(synthetic["metadata"]["childID"], child);
    assert_eq!(synthetic["metadata"]["state"], "failed");
    assert!(
        synthetic["text"]
            .as_str()
            .expect("synthetic text")
            .contains(&format!("sessionID=\"{child}\" state=\"failed\""))
    );
    let child_message = harness
        .store
        .get_message(child, "msg_child_stale")
        .await
        .expect("child message")
        .expect("child row");
    assert_eq!(child_message["finish"], "error");
    assert_eq!(child_message["content"][0]["state"]["status"], "error");

    // A second boot must not duplicate the settlement.
    let runtime = harness.restart();
    runtime.start().await.expect("second start");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let synthetics = harness
        .messages()
        .await
        .iter()
        .filter(|message| message["type"] == "synthetic")
        .count();
    assert_eq!(synthetics, 1);
}

/// A child that answered with a huge final text must not replay all of it into
/// the parent's repaired tool part: recovery goes through the same capped
/// report builder as a live completion (`subagent/protocol.rs`).
#[tokio::test]
async fn recovered_child_report_is_capped_in_the_repaired_tool_part() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"report_char_limit": 4000}}),
        None,
    )
    .await;
    let child = "ses_settled_child";
    create_child(&harness, &harness.session, child, "general").await;
    let now = oc_types::now_millis();
    harness
        .store
        .mark_session_idle(child, now, "succeeded")
        .await
        .expect("child settled");
    let long_answer = "a".repeat(10_000);
    harness
        .store
        .append_message(NewMessage {
            id: "msg_child_long".to_owned(),
            session_id: child.to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now, "streamed": now, "completed": now},
                "agent": "general",
                "model": {"providerID": "test", "id": "alpha"},
                "finish": "stop",
                "content": [{"type": "text", "text": long_answer}]
            }),
            time_created: now,
        })
        .await
        .expect("child answer");
    harness
        .store
        .append_message(NewMessage {
            id: "msg_parent_running".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "engineer",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_subagent",
                    "name": "subagent",
                    "state": {
                        "status": "running",
                        "input": {"description": "child task", "prompt": "do the work"},
                        "metadata": {"sessionID": child, "status": "running"}
                    },
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("parent message");

    let runtime = harness.restart();
    runtime.start().await.expect("start");

    let parent = harness
        .store
        .get_message(&harness.session, "msg_parent_running")
        .await
        .expect("read parent")
        .expect("parent row");
    let text = parent["content"][0]["state"]["content"][0]["text"]
        .as_str()
        .expect("recovered report text")
        .to_owned();
    // `</subagent>` is 11 chars, the open tag is 58 for this id/state, and the
    // marker is 135 chars for `ses_settled_child`; 4000 + 135 + 69 = 4204.
    assert!(
        text.chars().count() < 4300,
        "recovered report kept {} chars: recovery bypasses the cap",
        text.chars().count()
    );
    assert!(
        text.contains("truncated: 6000 chars omitted"),
        "{}",
        &text[4000.min(text.len() - 120)..]
    );
    assert!(
        text.starts_with("<subagent sessionID=\"ses_settled_child\" state=\"completed\">"),
        "{}",
        &text[..80.min(text.len())]
    );
    assert_eq!(text.chars().count(), 4000 + 135 + 69);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn crash_recovery_marks_crashed_sessions_idle_and_repairs_child_before_parent() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let child = "ses_fg_crashed_child";
    create_child(&harness, &harness.session, child, "general").await;
    let now = oc_types::now_millis();

    // Parent had a prior settled turn (`succeeded`), then started a new turn,
    // called a foreground subagent, and crashed before either finished.
    harness
        .store
        .append_event(NewEvent {
            id: "evt_parent_turn1_done".to_owned(),
            aggregate_id: harness.session.clone(),
            kind: "session.execution.succeeded".to_owned(),
            data: json!({ "sessionID": harness.session }),
            durable: true,
            version: 1,
            created: now,
        })
        .await
        .expect("prior terminal");
    harness
        .store
        .append_event(NewEvent {
            id: "evt_parent_turn2_start".to_owned(),
            aggregate_id: harness.session.clone(),
            kind: "session.execution.started".to_owned(),
            data: json!({ "sessionID": harness.session }),
            durable: true,
            version: 1,
            created: now + 1,
        })
        .await
        .expect("turn 2 started");
    harness
        .store
        .mark_session_busy(&harness.session, now + 10)
        .await
        .expect("parent busy");
    harness
        .store
        .mark_session_busy(child, now + 5)
        .await
        .expect("child busy");
    // Bump parent `time_updated` after child so DESC order visits parent first
    // unless recovery orders children before parents.
    harness
        .store
        .patch_session(
            &harness.session,
            oc_store::SessionPatch {
                title: Some("parent newer".to_owned()),
                ..oc_store::SessionPatch::default()
            },
        )
        .await
        .expect("touch parent");

    harness
        .store
        .append_message(NewMessage {
            id: "msg_parent_fg".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now + 10},
                "agent": "engineer",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_fg",
                    "name": "subagent",
                    "state": {
                        "status": "running",
                        "input": {
                            "description": "fg child",
                            "agent": "general",
                            "prompt": "do work",
                            "background": false
                        },
                        "metadata": {"sessionID": child, "status": "running"}
                    },
                    "time": {"created": now + 10}
                }]
            }),
            time_created: now + 10,
        })
        .await
        .expect("parent msg");
    harness
        .store
        .append_message(NewMessage {
            id: "msg_child_fg".to_owned(),
            session_id: child.to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now + 5},
                "agent": "general",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{"type": "text", "text": "partial child work"}]
            }),
            time_created: now + 5,
        })
        .await
        .expect("child msg");

    let runtime = harness.restart();
    runtime.start().await.expect("start after crash");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both child and parent must now be idle (`interrupted`), never stuck `busy`.
    let child_row = harness
        .store
        .get_session(child)
        .await
        .expect("read child")
        .expect("child exists");
    assert_eq!(child_row["outcome"], "interrupted");
    assert!(child_row["time"]["idle"].is_i64());
    let child_node = harness
        .store
        .session_node(child)
        .await
        .expect("child node")
        .expect("node exists");
    assert_eq!(child_node.status, "interrupted");
    assert_eq!(child_node.idle_reason.as_deref(), Some("shutdown"));
    assert_eq!(
        harness
            .store
            .active_child_count(&harness.session)
            .await
            .expect("active children"),
        0
    );

    let parent_row = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("read parent")
        .expect("parent exists");
    assert_eq!(parent_row["outcome"], "interrupted");
    assert!(parent_row["time"]["idle"].is_i64());

    // Parent's second turn must receive its own `session.execution.interrupted`
    // terminal after `session.execution.started`.
    assert_eq!(
        terminals(&harness.store, &harness.session).await,
        vec![
            "session.execution.succeeded".to_owned(),
            "session.execution.interrupted".to_owned(),
        ]
    );

    // Parent's foreground `subagent` tool part sees the child's cancelled outcome
    // and no duplicate synthetic failure is injected into parent's inbox.
    let parent_msg = harness
        .store
        .get_message(&harness.session, "msg_parent_fg")
        .await
        .expect("read parent msg")
        .expect("parent msg exists");
    let tool_err = parent_msg["content"][0]["state"]["error"]["message"]
        .as_str()
        .expect("repaired tool error message");
    assert!(
        tool_err.contains(&format!("sessionID=\"{child}\" state=\"cancelled\"")),
        "{tool_err}"
    );
    assert!(
        harness.inbox().await.is_empty(),
        "foreground child must not double-deliver"
    );
    assert_eq!(
        harness
            .messages()
            .await
            .iter()
            .filter(|message| message["type"] == "synthetic")
            .count(),
        0
    );
}

#[tokio::test]
async fn crash_recovery_marks_busy_session_with_no_messages_idle() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_event(NewEvent {
            id: "evt_started_only".to_owned(),
            aggregate_id: harness.session.clone(),
            kind: "session.execution.started".to_owned(),
            data: json!({ "sessionID": harness.session }),
            durable: true,
            version: 1,
            created: now,
        })
        .await
        .expect("started");
    harness
        .store
        .mark_session_busy(&harness.session, now)
        .await
        .expect("busy");

    let runtime = harness.restart();
    runtime.start().await.expect("start");

    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("read session")
        .expect("session exists");
    assert_eq!(session["outcome"], "interrupted");
    assert!(session["time"]["idle"].is_i64());
    assert_eq!(
        terminals(&harness.store, &harness.session).await,
        vec!["session.execution.interrupted".to_owned()]
    );
}
