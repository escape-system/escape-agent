//! Peer communication (`talk`), `roster` shape, descendant `session_write`, and `digest`.

use std::sync::Arc;

use oc_store::NewMessage;
use serde_json::{Value, json};

use crate::support::*;

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
    // `peer` is not in the wire `Session.Inbox.Item` union; `talk` writes the
    // wire-valid `synthetic` kind and keeps the sender metadata, so a client
    // that only knows the upstream four kinds can still render the queued row.
    assert_eq!(inbox[0]["type"], "synthetic");
    assert_eq!(inbox[0]["payload"]["from"], a.as_str());
    assert_eq!(inbox[0]["payload"]["metadata"]["source"], "peer");
    assert_eq!(inbox[0]["payload"]["kind"], "note");
    assert_eq!(inbox[0]["payload"]["agent"], "general");

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
    let steer_res = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"to": b, "kind": "steer", "text": "wrap up soon"}),
        )
        .await
        .expect("talk normalizes kind=steer to note");
    assert_eq!(
        steer_res.output.as_deref(),
        Some(format!("note delivered to {b}").as_str())
    );

    // Omitting `to` from a child session defaults to its parent session.
    let parent_res = tool
        .execute(
            &tool_context(&harness, &a),
            json!({"kind": "answer", "text": "done"}),
        )
        .await
        .expect("child talk without `to` defaults to parent");
    assert_eq!(
        parent_res.output.as_deref(),
        Some(format!("answer delivered to {}", harness.session).as_str())
    );

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
