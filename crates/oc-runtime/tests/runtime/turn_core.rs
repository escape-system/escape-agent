//! Core turn-loop behaviour: persistence, tools, permissions, steps and compaction.

use std::sync::Arc;
use std::time::Duration;

use oc_llm::{LlmEvent, Role};
use oc_store::{Delivery, NewCredential, NewInbox, NewMessage};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::support::*;

#[tokio::test]
async fn text_only_turn_persists_and_publishes() {
    let harness = Harness::new(
        vec![text_script("Hello from the model")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    assert!(harness.sink.has("session.execution.started"));
    assert!(harness.sink.has("session.execution.succeeded"));
    assert!(harness.sink.has("session.step.started"));
    assert!(harness.sink.has("session.step.streamed"));
    assert!(harness.sink.has("session.step.ended"));
    assert_eq!(harness.sink.count("session.text.delta"), 1);
    assert!(harness.sink.has("session.text.ended"));
    assert!(harness.sink.has("session.usage.updated"));

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0]["content"][0]["type"], "text");
    assert_eq!(assistants[0]["content"][0]["text"], "Hello from the model");
    assert_eq!(assistants[0]["finish"], "stop");
    assert!(assistants[0]["time"]["completed"].is_i64());

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].system.len(), 2);
    assert!(requests[0].system[1].contains("<env>"));
    assert!(!requests[0].tools.is_empty());
    let read = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "read")
        .expect("read tool");
    let required = read.input_schema["required"].as_array().unwrap();
    assert!(required.iter().any(|value| value == "expectation"));
}

#[tokio::test]
async fn stored_credentials_reach_the_provider_request() {
    let harness = Harness::new(
        vec![
            text_script("keyed"),
            text_script("unkeyed"),
            text_script("oauth"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    harness
        .store
        .create_credential(NewCredential {
            id: Some("cred_runtime".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "key", "key": "sk-stored"}),
            active: Some(true),
        })
        .await
        .expect("credential");
    harness.prompt("hi").await;
    assert_eq!(
        harness.model.requests()[0].options.api_key.as_deref(),
        Some("sk-stored")
    );

    harness
        .store
        .delete_credential("cred_runtime")
        .await
        .expect("delete");
    harness.prompt("again").await;
    assert_eq!(harness.model.requests()[1].options.api_key, None);

    harness
        .store
        .create_credential(NewCredential {
            id: Some("cred_oauth".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "oauth", "access_token": "oauth-token"}),
            active: Some(true),
        })
        .await
        .expect("oauth credential");
    harness.prompt("oauth please").await;
    assert_eq!(
        harness.model.requests()[2].options.api_key.as_deref(),
        Some("oauth-token")
    );
}

#[tokio::test]
async fn tool_call_runs_then_second_step_answers() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read the file").await;

    assert!(harness.sink.has("session.tool.called"));
    assert!(harness.sink.has("session.tool.success"));
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 2);

    let requests = harness.model.requests();
    let second = &requests[1];
    let result = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("tool result");
    let text = match &result.content[0] {
        oc_llm::ContentPart::ToolResult { content, .. } => content,
        other => panic!("unexpected part {other:?}"),
    };
    assert!(text.contains("hello world"), "got {text:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0]["content"][1]["state"]["status"], "completed");
    assert_eq!(
        assistants[0]["content"][1]["state"]["input"]["path"],
        "hello.txt"
    );
}

#[tokio::test]
async fn tool_error_continues_to_a_final_answer() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "missing.txt", "expectation": expectation()}),
            ),
            text_script("I could not read it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read missing").await;

    assert!(harness.sink.has("session.tool.failed"));
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 2);
    let requests = harness.model.requests();
    let result = requests[1]
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("tool result");
    match &result.content[0] {
        oc_llm::ContentPart::ToolResult { is_error, .. } => assert!(*is_error),
        other => panic!("unexpected part {other:?}"),
    }
}

#[tokio::test]
async fn permission_deny_becomes_a_tool_error() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("permission was denied"),
        ],
        Arc::new(DenyAll),
        json!({}),
        Some(json!([{"action": "read", "resource": "*", "effect": "ask"}])),
    )
    .await;
    harness.prompt("read anyway").await;

    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["error"]["type"], "PermissionDeniedError");
    assert_eq!(failures[0]["error"]["message"], "no tools for you");
    assert_eq!(failures[0]["executed"], false);
    assert!(!harness.sink.has("session.tool.success"));
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn interrupt_mid_stream_runs_the_finalizer() {
    let harness = Harness::new(
        vec![Script::HangAfter(vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart,
            LlmEvent::TextDelta {
                delta: "partial".to_owned(),
            },
        ])],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.enqueue("long answer").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness.wait_for_event("session.text.delta").await;

    assert!(
        harness
            .runtime
            .interrupt(&harness.session)
            .await
            .expect("interrupt")
    );
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert!(harness.sink.has("session.text.ended"));
    assert!(harness.sink.has("session.step.failed"));
    assert!(harness.sink.has("session.execution.interrupted"));
    assert!(!harness.sink.has("session.execution.succeeded"));
    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted[0]["reason"], "user");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants[0]["error"]["type"], "MessageAbortedError");
    assert_eq!(assistants[0]["finish"], "error");
    assert!(assistants[0]["time"]["completed"].is_i64());
}

#[tokio::test]
async fn max_steps_makes_one_final_tool_less_call() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            tool_script(
                "call_2",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            tool_script(
                "call_3",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
        ],
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("keep going").await;

    assert_eq!(harness.model.calls(), 3);
    let requests = harness.model.requests();
    assert!(
        !requests[2].tools.is_empty(),
        "tool definitions are preserved on final_call so the provider prompt cache stays valid"
    );
    assert_eq!(requests[2].tools, requests[0].tools);
    assert_eq!(requests[2].options.tool_choice.as_deref(), Some("none"));
    assert_eq!(harness.sink.count("session.tool.success"), 2);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn length_finish_reason_triggers_output_limit_continuation() {
    let harness = Harness::new(
        vec![
            Script::Events(vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: "part 1 ".to_owned(),
                },
                LlmEvent::TextEnd {
                    text: "part 1 ".to_owned(),
                },
                LlmEvent::Finish {
                    reason: "length".to_owned(),
                    usage: usage(),
                },
            ]),
            text_script("part 2 complete"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("write a long file").await;

    assert_eq!(
        harness.model.calls(),
        2,
        "truncated length step automatically continues"
    );
    let requests = harness.model.requests();
    assert!(
        request_contains(
            &requests[1],
            "cut off because it hit the output token limit"
        ),
        "continuation prompt is injected"
    );
    assert_eq!(
        requests[0].options.cache_key.as_deref(),
        Some(harness.session.as_str())
    );
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn two_prompts_serialize() {
    let harness = Harness::new(
        vec![text_script("first"), text_script("second")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.enqueue("one").await;
    harness.enqueue("two").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert_eq!(harness.sink.count("session.execution.started"), 2);
    assert_eq!(harness.sink.count("session.execution.succeeded"), 2);
    assert_eq!(harness.model.calls(), 2);
    let messages = harness.messages().await;
    assert_eq!(assistants(&messages).len(), 2);
    let kinds = harness.sink.kinds();
    let starts: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| *kind == "session.execution.started")
        .map(|(index, _)| index)
        .collect();
    let done: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| *kind == "session.execution.succeeded")
        .map(|(index, _)| index)
        .collect();
    assert!(starts[0] < done[0]);
    assert!(done[0] < starts[1]);
    assert!(starts[1] < done[1]);
}

#[tokio::test]
async fn reopened_runtime_sees_prior_context() {
    let harness = Harness::new(
        vec![text_script("one"), text_script("two")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("first prompt").await;
    harness.runtime.shutdown().await;

    let reopened = harness.restart();
    harness.enqueue("second prompt").await;
    reopened.notify(&harness.session).await.expect("notify");
    reopened
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    let texts: Vec<String> = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(oc_llm::ContentPart::as_text)
        .map(str::to_owned)
        .collect();
    assert!(texts.iter().any(|text| text == "first prompt"));
    assert!(texts.iter().any(|text| text == "one"));
    assert!(texts.iter().any(|text| text == "second prompt"));
}

#[tokio::test]
async fn expectation_is_injected_and_stripped() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("done"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read").await;

    let requests = harness.model.requests();
    let schema = &requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "read")
        .expect("read")
        .input_schema;
    assert_eq!(
        schema["properties"]["expectation"]["required"],
        json!(["why", "expect_seconds", "intervene_after_seconds"])
    );
    let called = harness.sink.data("session.tool.called");
    assert!(called[0]["input"].get("expectation").is_none());

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    let input = &assistants[0]["content"][1]["state"]["input"];
    assert_eq!(input["path"], "hello.txt");
    assert!(input.get("expectation").is_none());
}

#[tokio::test]
async fn host_materialized_prompt_is_not_duplicated() {
    let harness = Harness::new(
        vec![text_script("answered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let id = harness.enqueue("host wrote me").await;
    harness
        .store
        .append_message(NewMessage {
            id: id.clone(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            data: json!({"text": "host wrote me", "time": {"created": oc_types::now_millis()}}),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("host message");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let messages = harness.messages().await;
    let users: Vec<&Value> = messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["id"], id);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn oversized_context_compacts_before_the_request() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer from the summary"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    // The fixture context window is 100k, so the 85k-token ceiling needs a
    // transcript above roughly 340k characters.
    let huge = "a".repeat(360_000);
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": huge}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert_eq!(harness.model.calls(), 2);
    let requests = harness.model.requests();
    assert!(
        !requests[0].tools.is_empty(),
        "compaction preserves tool definitions and system prefix for KV cache reuse"
    );
    assert_eq!(requests[0].tools, requests[1].tools);
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(requests[0].options.tool_choice.as_deref(), Some("none"));
    let summary_seen = requests[1].messages.iter().any(|message| {
        message.content.iter().any(|part| {
            part.as_text()
                .is_some_and(|text| text.contains("handoff summary"))
        })
    });
    assert!(summary_seen);

    let messages = harness.messages().await;
    let compaction = messages
        .iter()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .expect("compaction row");
    assert_eq!(compaction["status"], "completed");
    assert!(compaction["summary"].as_str().unwrap().contains("handoff"));
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn auto_compaction_records_reason_auto() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer from the summary"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let trigger = MessageId::new().into_inner();
    // The fixture context window is 100k, so the 85k-token ceiling needs a
    // transcript above roughly 340k characters.
    harness
        .store
        .enqueue(NewInbox {
            id: trigger.clone(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "a".repeat(360_000)}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert_eq!(harness.model.calls(), 2, "summary then the real request");
    let messages = harness.messages().await;
    let compaction = messages
        .iter()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .expect("compaction row");
    assert_eq!(compaction["status"], "completed");
    assert_eq!(compaction["reason"], "auto");
    assert_eq!(
        compaction["keepID"], trigger,
        "the huge triggering prompt is kept verbatim"
    );
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn auto_compaction_publishes_durable_lifecycle_events() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer from the summary"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "a".repeat(360_000)}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    let started = logged
        .iter()
        .find(|event| event.kind == "session.compaction.started")
        .expect("durable session.compaction.started persisted");
    let ended = logged
        .iter()
        .find(|event| event.kind == "session.compaction.ended")
        .expect("durable session.compaction.ended persisted");
    assert!(
        started.seq < ended.seq,
        "started commits before ended: {} < {}",
        started.seq,
        ended.seq
    );
    assert_eq!(started.version, 1);
    assert_eq!(ended.version, 1);
    assert_eq!(started.data["sessionID"], harness.session);
    assert_eq!(ended.data["sessionID"], harness.session);
    assert_eq!(started.data["reason"], "auto");
    assert_eq!(ended.data["reason"], "auto");

    // The started event names the compaction message whose row was inserted
    // before it was published.
    let compaction_id = harness
        .messages()
        .await
        .iter()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .and_then(|message| message.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .expect("compaction row");
    assert_eq!(started.data["inputID"], compaction_id);
    assert_eq!(
        ended.data["text"], "## Objective\nhandoff summary",
        "ended carries the stored summary"
    );
    assert!(
        !started.data.to_string().contains("null"),
        "started omits its optional inputID when absent and never emits null: {}",
        started.data
    );

    // Published only after commit: every durable envelope carries the seq that
    // the log allocated for the same event id.
    for (kind, stored) in [
        ("session.compaction.started", started),
        ("session.compaction.ended", ended),
    ] {
        let envelope = harness
            .sink
            .events
            .lock()
            .expect("events")
            .iter()
            .find(|event| event.kind == kind && event.id.as_str() == stored.id)
            .cloned()
            .unwrap_or_else(|| panic!("{kind} published"));
        let durable = envelope.durable.expect("compaction events are durable");
        assert_eq!(
            durable.seq, stored.seq,
            "{kind} publishes its committed seq"
        );
    }
}

#[tokio::test]
async fn small_context_writes_no_compaction_row() {
    let harness = Harness::new(vec![text_script("ok")], Arc::new(AllowAll), json!({}), None).await;
    harness.prompt("a short prompt").await;

    assert_eq!(harness.model.calls(), 1);
    let messages = harness.messages().await;
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
            .count(),
        0,
        "a small context stays uncompacted"
    );
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn crash_recovery_repairs_rows_and_wakes_inbox() {
    let harness = Harness::new(
        vec![text_script("recovered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_message(NewMessage {
            id: "msg_stale".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "build",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_stale",
                    "name": "read",
                    "state": {"status": "running", "input": {"path": "hello.txt"}},
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("stale assistant");
    harness.enqueue("after the crash").await;

    harness.runtime.start().await.expect("start");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let stale = harness
        .store
        .get_message(&harness.session, "msg_stale")
        .await
        .expect("read stale")
        .expect("stale exists");
    assert_eq!(stale["finish"], "error");
    assert_eq!(stale["error"]["type"], "MessageAbortedError");
    assert!(stale["time"]["completed"].is_i64());
    assert_eq!(stale["content"][0]["state"]["status"], "error");
    assert_eq!(
        stale["content"][0]["state"]["error"]["type"],
        "ToolAbortedError"
    );

    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted[0]["reason"], "shutdown");
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 1);
    assert!(harness.work.join("hello.txt").exists());
}

#[tokio::test]
async fn recovery_fails_a_running_compaction_and_preserves_its_reason() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_message(NewMessage {
            id: "msg_running_compaction".to_owned(),
            session_id: harness.session.clone(),
            kind: "compaction".to_owned(),
            data: json!({
                "time": {"created": now},
                "status": "running",
                "reason": "auto",
                "summary": "",
                "keepID": "msg_trigger"
            }),
            time_created: now,
        })
        .await
        .expect("running compaction");

    harness.runtime.start().await.expect("start");
    let repaired = harness
        .store
        .get_message(&harness.session, "msg_running_compaction")
        .await
        .expect("read compaction")
        .expect("compaction exists");
    assert_eq!(repaired["status"], "failed");
    assert_eq!(
        repaired["reason"], "auto",
        "recovery keeps the compaction reason"
    );
    assert_eq!(repaired["error"]["type"], "CompactionAbortedError");
    assert!(
        repaired["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("restarted")),
        "the abort error explains the restart: {}",
        repaired["error"]["message"]
    );
    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0]["reason"], "shutdown");
    assert_eq!(
        harness.model.calls(),
        0,
        "recovery must not run the compaction summary"
    );

    // A second boot finds the row already failed and stays quiet.
    let second = harness.restart();
    second.start().await.expect("second start");
    assert_eq!(
        harness.sink.data("session.execution.interrupted").len(),
        1,
        "the interrupted event is not republished"
    );
    assert_eq!(harness.model.calls(), 0);
}

#[tokio::test]
async fn recovery_is_idempotent_for_assistant_rows() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_message(NewMessage {
            id: "msg_partial_assistant".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "build",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_running",
                    "name": "read",
                    "state": {"status": "running", "input": {"path": "hello.txt"}},
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("partial assistant");

    harness.runtime.start().await.expect("start");
    let repaired = harness
        .store
        .get_message(&harness.session, "msg_partial_assistant")
        .await
        .expect("read assistant")
        .expect("assistant exists");
    let completed = repaired["time"]["completed"].clone();
    assert!(completed.is_i64(), "recovery stamps completion: {repaired}");
    assert_eq!(repaired["finish"], "error");
    assert_eq!(repaired["error"]["type"], "MessageAbortedError");
    assert_eq!(harness.sink.data("session.execution.interrupted").len(), 1);

    let second = harness.restart();
    second.start().await.expect("second start");
    let again = harness
        .store
        .get_message(&harness.session, "msg_partial_assistant")
        .await
        .expect("read assistant again")
        .expect("assistant exists");
    assert_eq!(
        again["time"]["completed"], completed,
        "a second recovery must not restamp the completion time"
    );
    assert_eq!(again["error"]["type"], "MessageAbortedError");
    assert_eq!(
        harness.sink.data("session.execution.interrupted").len(),
        1,
        "the second boot publishes no new interrupted event"
    );
    assert_eq!(harness.model.calls(), 0);
}

#[tokio::test]
async fn shutdown_preserves_a_queued_row_for_the_restarted_runtime() {
    let harness = Harness::new(
        vec![
            Script::HangAfter(vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: "partial".to_owned(),
                },
            ]),
            text_script("after the restart"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.enqueue("long answer").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness.wait_for_event("session.text.delta").await;

    let queued = harness
        .enqueue_with("QUEUED_AFTER_RESTART: follow up", Delivery::Queue)
        .await;
    harness.runtime.shutdown().await;

    let restarted = harness.restart();
    restarted.start().await.expect("start after shutdown");
    restarted
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request_contains(request, "QUEUED_AFTER_RESTART"))
            .count(),
        1,
        "the queued row is delivered by exactly one provider request"
    );
    let messages = harness.messages().await;
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                message.get("type").and_then(Value::as_str) == Some("user")
                    && message
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains("QUEUED_AFTER_RESTART"))
            })
            .count(),
        1,
        "the queued row becomes exactly one user message"
    );
    assert_eq!(harness.delivery_count(&queued), 1);
    assert!(harness.inbox().await.is_empty());
    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0]["reason"], "shutdown");
}

#[tokio::test]
async fn malformed_tool_arguments_become_a_tool_error_and_turn_recovers() {
    let harness = Harness::new(
        vec![
            Script::Events(vec![
                LlmEvent::StepStart,
                LlmEvent::ToolInputStart {
                    id: "call_bad_json".to_owned(),
                    name: "read".to_owned(),
                },
                LlmEvent::ToolInputEnd {
                    id: "call_bad_json".to_owned(),
                    text: "{\"path\": \"hello.txt\"".to_owned(),
                },
                LlmEvent::ToolCall {
                    id: "call_bad_json".to_owned(),
                    name: "read".to_owned(),
                    input: Value::String("{\"path\": \"hello.txt\"".to_owned()),
                },
                LlmEvent::Finish {
                    reason: "tool-calls".to_owned(),
                    usage: usage(),
                },
            ]),
            text_script("recovered after malformed tool json"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read hello.txt").await;

    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["error"]["type"], "InvalidToolArgumentsError");
    assert_eq!(failures[0]["executed"], false);
    assert!(harness.sink.has("session.execution.succeeded"));
    assert!(!harness.sink.has("session.execution.failed"));
}

#[tokio::test]
async fn verify_on_stop_nudges_when_code_file_changed_without_test() {
    let harness = Harness::new(
        vec![
            // Step 1: LLM writes to a code file (src/main.rs)
            named_tool_script(
                "write",
                "call_write_code",
                json!({
                    "path": "src/main.rs",
                    "content": "fn main() {}\n",
                    "expectation": expectation(),
                }),
            ),
            // Step 2: LLM tries to stop with text without running tests -> runtime intercepts and nudges
            text_script("I have written the code!"),
            // Step 3: LLM receives verification nudge and runs cargo test via shell
            named_tool_script(
                "shell",
                "call_run_test",
                json!({
                    "command": "echo cargo test ok",
                    "expectation": expectation(),
                }),
            ),
            // Step 4: LLM summarizes test pass and finishes cleanly
            text_script("Tests passed successfully."),
        ],
        Arc::new(AllowAll),
        json!({"verify_on_stop": true}),
        None,
    )
    .await;

    harness.prompt("implement main function").await;

    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 4);

    let requests = harness.model.requests();
    // Request 3 should have received the synthetic verification nudge
    let third_req = &requests[2];
    let nudge_msg = third_req.messages.iter().find(|m| {
        m.role == Role::User
            && m.content.iter().any(|c| match c {
                oc_llm::ContentPart::Text { text } => text.contains("verification evidence"),
                _ => false,
            })
    });
    assert!(
        nudge_msg.is_some(),
        "third request must contain verification nudge"
    );
}

#[tokio::test]
async fn verify_on_stop_stops_after_nudge_cap_if_unverified() {
    let harness = Harness::new(
        vec![
            // Step 1: LLM writes code
            named_tool_script(
                "write",
                "call_write_code",
                json!({
                    "path": "src/main.rs",
                    "content": "fn main() {}\n",
                    "expectation": expectation(),
                }),
            ),
            // Step 2: LLM tries to stop -> Nudge 1
            text_script("Done without test."),
            // Step 3: LLM tries to stop again -> Nudge 2
            text_script("Still no test."),
            // Step 4: LLM tries to stop again -> Nudge cap reached (2), settles successfully
            text_script("I cannot test right now."),
        ],
        Arc::new(AllowAll),
        json!({"verify_on_stop": true}),
        None,
    )
    .await;

    harness.prompt("implement main function").await;

    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 4);
}

#[tokio::test]
async fn stall_guard_nudges_on_identical_consecutive_calls() {
    let harness = Harness::new(
        vec![
            // Step 1: LLM reads hello.txt
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            // Step 2: LLM reads hello.txt again (identical call 2)
            tool_script(
                "call_2",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            // Step 3: LLM reads hello.txt again (identical call 3 -> StallGuard triggers warning)
            tool_script(
                "call_3",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            // Step 4: Next request receives Stall Guard Warning, agent answers with text
            text_script("I will break the loop and proceed with a different plan."),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    harness.prompt("investigate hello.txt").await;

    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 4);

    let requests = harness.model.requests();
    let fourth_req = &requests[3];
    let warning_msg = fourth_req.messages.iter().find(|m| {
        m.role == Role::User
            && m.content.iter().any(|c| match c {
                oc_llm::ContentPart::Text { text } => text.contains("Stall Guard Warning"),
                _ => false,
            })
    });
    assert!(
        warning_msg.is_some(),
        "fourth request must contain stall guard warning notice"
    );
}

#[tokio::test]
async fn repetition_guard_suppresses_length_continuation_on_loop() {
    let bad_text = "echo 'repetitive content inside the stream';\n".repeat(25);
    let harness = Harness::new(
        vec![
            // Step 1: Degenerate stream finishes with length reason
            Script::Events(vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: bad_text.clone(),
                },
                LlmEvent::TextEnd { text: bad_text },
                LlmEvent::Finish {
                    reason: "length".to_owned(),
                    usage: usage(),
                },
            ]),
            // If continuation was NOT suppressed, step 2 would run.
            text_script("should not be reached"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    harness.prompt("generate repetition").await;

    // Must settle in 1 step because repetition guard suppressed continuation!
    assert_eq!(
        harness.model.calls(),
        1,
        "repetition guard must suppress length continuation on degenerate loop"
    );
    assert!(harness.sink.has("session.execution.succeeded"));
}

// ---------------------------------------------------------------------------
// Observer wiring
// ---------------------------------------------------------------------------
