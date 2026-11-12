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
    let step_limit = harness.sink.data("session.execution.step_limit");
    assert_eq!(
        step_limit.len(),
        1,
        "the exhausted step budget is reported exactly once: {step_limit:?}"
    );
    assert_eq!(step_limit[0]["steps"], 2);
    assert_eq!(step_limit[0]["sessionID"], harness.session);
    assert!(
        !step_limit[0].to_string().contains("null"),
        "the qualifier payload has exactly two fields and never null: {}",
        step_limit[0]
    );
}

#[tokio::test]
async fn explicit_ceiling_guards_even_when_auto_is_false() {
    // 100k window -> computed ceiling 84k; the explicit 30k ceiling lowers it,
    // and the default buffer (max(8k, 10%)) puts the trigger at 20k. A ~25k
    // token prompt must compact even though `auto` is absent/false.
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer from the summary"),
        ],
        Arc::new(AllowAll),
        json!({"compaction": {"ceiling_tokens": 30_000}}),
        None,
    )
    .await;
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "a".repeat(100_000)}),
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
    let compactions: Vec<&Value> = messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .collect();
    assert_eq!(compactions.len(), 1, "exactly one compaction row");
    assert_eq!(compactions[0]["status"], "completed");
    assert_eq!(
        compactions[0]["reason"], "auto",
        "the wire reason stays `auto` for the internal `guard` path"
    );
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn buffer_moves_the_trigger_earlier() {
    // Same transcript and explicit 50k ceiling; only the buffer differs.
    // trigger = 50k - buffer: 42k (no compaction) vs 10k (compaction).
    let text = "a".repeat(60_000);

    let small = Harness::new(
        vec![text_script("answer")],
        Arc::new(AllowAll),
        json!({"compaction": {"ceiling_tokens": 50_000, "buffer": 8_000}}),
        None,
    )
    .await;
    small
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: small.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": text}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    small.runtime.notify(&small.session).await.expect("notify");
    small
        .runtime
        .wait_idle(&small.session, Duration::from_secs(10))
        .await
        .expect("idle");
    assert_eq!(
        small.model.calls(),
        1,
        "a small buffer keeps the trigger above the estimate"
    );
    assert!(
        !small
            .messages()
            .await
            .iter()
            .any(|message| message.get("type").and_then(Value::as_str) == Some("compaction")),
        "no compaction row below the trigger"
    );

    let large = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer from the summary"),
        ],
        Arc::new(AllowAll),
        json!({"compaction": {"ceiling_tokens": 50_000, "buffer": 40_000}}),
        None,
    )
    .await;
    large
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: large.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": text}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    large.runtime.notify(&large.session).await.expect("notify");
    large
        .runtime
        .wait_idle(&large.session, Duration::from_secs(10))
        .await
        .expect("idle");
    assert_eq!(
        large.model.calls(),
        2,
        "a large buffer lowers the trigger below the estimate"
    );
    assert_eq!(
        large
            .messages()
            .await
            .iter()
            .filter(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
            .count(),
        1,
        "exactly one compaction row above the trigger"
    );
}

#[tokio::test]
async fn step_limit_emits_a_durable_qualifier() {
    // steps=2: two tool-enabled steps run, then the final request is made with
    // tools disabled even though the model still asks for one. The turn
    // succeeds, and the qualifier is a separate durable event ordered after it.
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
    assert!(harness.sink.has("session.execution.succeeded"));

    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    let succeeded = logged
        .iter()
        .find(|event| event.kind == "session.execution.succeeded")
        .expect("durable terminal succeeded");
    let qualified = logged
        .iter()
        .find(|event| event.kind == "session.execution.step_limit")
        .expect("durable step-limit qualifier");
    assert!(
        qualified.seq > succeeded.seq,
        "the qualifier is committed after the terminal: {} > {}",
        qualified.seq,
        succeeded.seq
    );
    assert_eq!(qualified.data["sessionID"], harness.session);
    assert_eq!(qualified.data["steps"], 2);
    let mut keys: Vec<&str> = qualified
        .data
        .as_object()
        .expect("payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["sessionID", "steps"],
        "the payload has exactly two keys and no null: {}",
        qualified.data
    );
    assert!(
        !qualified.data.to_string().contains("null"),
        "the payload never emits null: {}",
        qualified.data
    );
    assert_eq!(qualified.version, 1);

    // The durable session row is honest about *why* it ended: the model did not
    // stop, the budget did. The stored `idle_outcome` is the internal literal
    // `step_limit` (the wire terminal stays `succeeded`), and `idle_reason`
    // carries the step-limit grammar alongside it.
    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(
        session["outcome"], "step_limit",
        "a step-limited turn stores the honest internal outcome: {session}"
    );
    let node = harness
        .store
        .session_node(&harness.session)
        .await
        .expect("session node")
        .expect("node row");
    let reason = oc_types::step_limit_reason(2);
    assert_eq!(
        node.idle_reason.as_deref(),
        Some(reason.as_str()),
        "the session row records the exhausted budget"
    );
    assert!(
        node.idle_reason
            .as_deref()
            .is_some_and(|value| value.starts_with("step limit reached after ")),
        "the reason uses the canonical step-limit grammar: {:?}",
        node.idle_reason
    );
}

#[tokio::test]
async fn a_model_that_stops_on_its_own_emits_no_step_limit_event() {
    // steps=2 and the model calls one tool, then answers with text. The turn
    // ends because the model stopped, not because the budget ran out, so the
    // qualifier must not be published at all.
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("all done"),
        ],
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("read once").await;

    assert_eq!(harness.model.calls(), 2);
    assert!(harness.sink.has("session.execution.succeeded"));
    assert!(
        !harness.sink.has("session.execution.step_limit"),
        "a voluntary stop publishes no qualifier"
    );
    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    assert!(
        !logged
            .iter()
            .any(|event| event.kind == "session.execution.step_limit"),
        "a voluntary stop writes no durable qualifier"
    );
}

#[tokio::test]
async fn a_model_that_stops_on_its_own_records_no_step_limit_reason() {
    // Same budget as the limited turn, but here the model answers with text
    // before the budget runs out. The session row must not claim a step limit:
    // no qualifier event and no durable `idle_reason`.
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("finished on my own"),
        ],
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("read once").await;

    assert_eq!(harness.model.calls(), 2, "the budget was never exhausted");
    assert!(harness.sink.has("session.execution.succeeded"));
    assert!(
        !harness.sink.has("session.execution.step_limit"),
        "a voluntary stop publishes no qualifier"
    );

    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(session["outcome"], "succeeded");
    let node = harness
        .store
        .session_node(&harness.session)
        .await
        .expect("session node")
        .expect("node row");
    assert_eq!(
        node.idle_reason, None,
        "a voluntary stop must not look step-limited"
    );
    assert_eq!(node.status, "completed");
}

#[tokio::test]
async fn step_limited_turn_still_publishes_the_succeeded_terminal() {
    // Guard against "fixing" the step limit by renaming the wire terminal:
    // `SessionOutcome` has three literals and `Runtime::parse_session`
    // deserializes strictly, so the terminal must stay
    // `session.execution.succeeded` and the qualifier rides alongside it.
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

    assert_eq!(harness.sink.count("session.execution.succeeded"), 1);
    assert_eq!(harness.sink.count("session.execution.step_limit"), 1);
    assert_eq!(
        harness.sink.count("session.execution.failed"),
        0,
        "a step-limited turn is not a failure"
    );
    assert_eq!(harness.sink.count("session.execution.interrupted"), 0);

    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    let kinds: Vec<&str> = logged
        .iter()
        .map(|event| event.kind.as_str())
        .filter(|kind| kind.starts_with("session.execution."))
        .collect();
    assert_eq!(
        kinds,
        [
            "session.execution.started",
            "session.execution.succeeded",
            "session.execution.step_limit",
        ],
        "the same execution carries both the wire terminal and the qualifier"
    );

    // The stored `step_limit` literal is unknowable to the three-literal wire
    // enum, so the read path must tolerate it: `SessionInfo` maps it to `None`
    // instead of failing. That is what lets the session run again.
    let session = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(session["outcome"], "step_limit");
    let info: oc_types::SessionInfo =
        serde_json::from_value(session).expect("a step-limited session stays parseable");
    assert_eq!(
        info.outcome, None,
        "an unknown stored outcome reads as absent, not as a parse failure"
    );
}

#[tokio::test]
async fn a_step_limited_session_can_run_again() {
    // The regression the strict parse would have caused: a stored fourth
    // literal makes every later execution of the session fail to parse.
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
            text_script("second turn ran"),
        ],
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("exhaust the budget").await;
    let limited = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("session row")
        .expect("session exists");
    assert_eq!(limited["outcome"], "step_limit");

    harness.prompt("run again").await;

    assert_eq!(
        harness.model.calls(),
        4,
        "the second turn reached the model"
    );
    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(
        assistants
            .last()
            .map(|message| &message["content"][0]["text"]),
        Some(&json!("second turn ran")),
        "the second turn produced its own answer"
    );
    assert!(
        assistants
            .last()
            .is_some_and(|message| message["error"].is_null()),
        "the second turn must not fail on the stored outcome"
    );
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
        json!({"compaction": {"auto": true}}),
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
async fn auto_false_writes_no_compaction_row() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer"),
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

    let messages = harness.messages().await;
    assert!(
        !messages
            .iter()
            .any(|message| message.get("type").and_then(Value::as_str) == Some("compaction")),
        "auto:false must not write a compaction row"
    );
    assert_eq!(harness.model.calls(), 1, "no summary request without auto");
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
        json!({"compaction": {"auto": true}}),
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
async fn compaction_does_not_keep_oversized_assistant_message_as_keep_id() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer after summarizing huge assistant step"),
        ],
        Arc::new(AllowAll),
        json!({"compaction": {"auto": true}}),
        None,
    )
    .await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_message(oc_store::NewMessage {
            id: "msg_user_before".to_owned(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            data: json!({"time": {"created": now}, "text": "inspect output"}),
            time_created: now,
        })
        .await
        .expect("append user");
    // An assistant message whose tool output alone exceeds both `keep_tokens`
    // (15k) and the compaction trigger (75k) must be summarized by compaction,
    // never preserved verbatim as `keepID`.
    harness
        .store
        .append_message(oc_store::NewMessage {
            id: "msg_huge_assistant".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now, "streamed": now, "completed": now},
                "agent": "build",
                "model": {"providerID": "test", "id": "alpha"},
                "finish": "tool-calls",
                "content": [{
                    "type": "tool",
                    "id": "call_huge",
                    "name": "read",
                    "executed": true,
                    "state": {
                        "status": "completed",
                        "input": {"filePath": "huge.txt"},
                        "content": [{"type": "text", "text": "x".repeat(360_000)}]
                    },
                    "time": {"created": now, "ran": now, "completed": now}
                }]
            }),
            time_created: now + 1,
        })
        .await
        .expect("append huge assistant");

    harness.prompt("continue").await;

    let messages = harness.messages().await;
    let compaction = messages
        .iter()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .expect("compaction row");
    assert_eq!(compaction["status"], "completed");
    assert_ne!(
        compaction["keepID"], "msg_huge_assistant",
        "oversized assistant message must not be kept as keepID across compaction"
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
        json!({"compaction": {"auto": true}}),
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
                    "command": "cargo test --help >/dev/null",
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
async fn verify_on_stop_failed_shell_test_does_not_mark_verified() {
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
            text_script("Implemented without tests."),
            // Step 3: Verification command exits non-zero -> must NOT count as verified
            named_tool_script(
                "shell",
                "call_fail_test",
                json!({
                    "command": "cargo test --help >/dev/null && false",
                    "expectation": expectation(),
                }),
            ),
            // Step 4: LLM tries to stop after failing test -> Nudge 2 (still unverified)
            text_script("Attempted test run."),
            // Step 5: Passing verification command clears verify_on_stop
            named_tool_script(
                "shell",
                "call_pass_test",
                json!({
                    "command": "cargo test --help >/dev/null",
                    "expectation": expectation(),
                }),
            ),
            // Step 6: LLM finishes cleanly
            text_script("Tests passed after fix."),
        ],
        Arc::new(AllowAll),
        json!({"verify_on_stop": true}),
        None,
    )
    .await;

    harness.prompt("implement main function").await;

    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 6);

    let requests = harness.model.requests();
    let fourth_req_nudged = requests[4].messages.iter().any(|m| {
        m.role == Role::User
            && m.content.iter().any(|c| match c {
                oc_llm::ContentPart::Text { text } => text.contains("verification evidence"),
                _ => false,
            })
    });
    assert!(
        fourth_req_nudged,
        "request after failed verification command must still carry verification nudge"
    );
}

#[tokio::test]
async fn verify_on_stop_enabled_by_default_rejects_echo_spoof_and_can_be_disabled() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_write_default",
                json!({
                    "path": "src/main.rs",
                    "content": "fn main() {}\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("Done."),
            named_tool_script(
                "shell",
                "call_fake_echo",
                json!({
                    "command": "echo cargo test ok",
                    "expectation": expectation(),
                }),
            ),
            text_script("Faked test with echo."),
            named_tool_script(
                "shell",
                "call_real_test",
                json!({
                    "command": "cargo test --help >/dev/null",
                    "expectation": expectation(),
                }),
            ),
            text_script("Verified for real."),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    harness.prompt("implement main").await;
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 6);

    let disabled = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_write_disabled",
                json!({
                    "path": "src/main.rs",
                    "content": "fn main() {}\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("Done without verifying."),
        ],
        Arc::new(AllowAll),
        json!({"verify_on_stop": false}),
        None,
    )
    .await;

    disabled.prompt("implement main").await;
    assert!(disabled.sink.has("session.execution.succeeded"));
    assert_eq!(disabled.model.calls(), 2);
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
async fn stall_guard_records_remaining_calls_after_warning_in_same_batch() {
    let multi_read = |calls: &[(&str, u64)]| {
        let mut events = vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart,
            LlmEvent::TextDelta {
                delta: "checking".to_owned(),
            },
            LlmEvent::TextEnd {
                text: "checking".to_owned(),
            },
        ];
        for (call_id, offset) in calls {
            let input =
                json!({"path": "hello.txt", "offset": offset, "expectation": expectation()});
            let text = input.to_string();
            events.push(LlmEvent::ToolInputStart {
                id: (*call_id).to_owned(),
                name: "read".to_owned(),
            });
            events.push(LlmEvent::ToolInputDelta {
                id: (*call_id).to_owned(),
                delta: text.clone(),
            });
            events.push(LlmEvent::ToolInputEnd {
                id: (*call_id).to_owned(),
                text,
            });
            events.push(LlmEvent::ToolCall {
                id: (*call_id).to_owned(),
                name: "read".to_owned(),
                input,
            });
        }
        events.push(LlmEvent::Finish {
            reason: "tool-calls".to_owned(),
            usage: usage(),
        });
        Script::Events(events)
    };

    let harness = Harness::new(
        vec![
            // Step 1: 3x identical offset=1 triggers warning AND trailing offset=2 must still be recorded (count=1)
            multi_read(&[("c1", 1), ("c2", 1), ("c3", 1), ("c4", 2)]),
            // Step 2: 2x offset=2 reaches count=3 only if c4 from Step 1 was recorded
            multi_read(&[("c5", 2), ("c6", 2)]),
            // Step 3: Settle
            text_script("Proceeding with a different approach."),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    harness.prompt("investigate hello.txt").await;

    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 3);

    let requests = harness.model.requests();
    for step_idx in [1usize, 2usize] {
        let has_warning = requests[step_idx].messages.iter().any(|m| {
            m.role == Role::User
                && m.content.iter().any(|c| match c {
                    oc_llm::ContentPart::Text { text } => text.contains("Stall Guard Warning"),
                    _ => false,
                })
        });
        assert!(
            has_warning,
            "request {} must contain stall guard warning notice",
            step_idx + 1
        );
    }
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
