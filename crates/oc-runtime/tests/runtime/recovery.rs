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
                "agent": "build",
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
                "agent": "build",
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
                "agent": "build",
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
                "agent": "build",
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
    let deadline = Instant::now() + Duration::from_secs(10);
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
