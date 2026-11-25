//! `session_write` per-sender rate budget.
//!
//! `talk` has been rate limited since the sibling tools landed; a write storm
//! had no bound at all. These tests pin the write budget: it counts accepted
//! writes per sender, refuses the write over the limit with a structured tool
//! error before any durable row exists, honors
//! `subagents.siblings.max_writes_per_min`, and leaves other senders alone.
//!
//! A write is materialised at admission, so its inbox doorbell and its
//! transcript message commit together under one id. The child actor may then
//! promote or consume the doorbell mid-test, so the durable total is the set of
//! *ids*: one accepted write is exactly one durable write, whether it is still
//! pending (doorbell + message) or delivered (message only).

use std::collections::HashSet;
use std::sync::Arc;

use oc_store::{MessageQuery, Order};
use serde_json::{Value, json};

use crate::support::*;

/// Durable writes the child holds from `session_write`, keyed by row id.
///
/// `enqueue_materialized` commits the pending doorbell and the transcript
/// message with the same id, and promotion flips that message in place instead
/// of inserting a second row. Counting rows would therefore count a pending
/// write twice, so each id contributes once; the transcript row wins because it
/// is read first and carries the full message shape.
async fn durable_writes(harness: &Harness, child: &str) -> Vec<Value> {
    let mut rows: Vec<(String, Value)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = None;
    loop {
        let page = harness
            .store
            .list_messages(MessageQuery {
                session_id: child.to_owned(),
                limit: 200,
                order: Order::Asc,
                cursor,
                kind: None,
            })
            .await
            .expect("messages");
        for message in page.items {
            let peer = message["type"] == "synthetic"
                && message.pointer("/metadata/source").and_then(Value::as_str) == Some("peer");
            if peer {
                let id = message_id(&message);
                if seen.insert(id.clone()) {
                    rows.push((id, message));
                }
            }
        }
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let inbox: Vec<Value> = harness.store.list_inbox(child).await.expect("inbox");
    for item in inbox {
        let id = message_id(&item);
        if seen.insert(id.clone()) {
            rows.push((id, item));
        }
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

fn message_id(row: &Value) -> String {
    row["id"]
        .as_str()
        .unwrap_or_else(|| panic!("row carries an id: {row}"))
        .to_owned()
}

/// Poll until the conserved durable total reaches `expected` (the child actor
/// may promote pending rows while the test runs).
async fn settle(harness: &Harness, child: &str, expected: usize) -> Vec<Value> {
    let mut rows = durable_writes(harness, child).await;
    for _ in 0..100 {
        if rows.len() >= expected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        rows = durable_writes(harness, child).await;
    }
    rows
}

#[tokio::test]
async fn session_write_budget_rejects_the_twenty_first_write() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let child = create_child(&harness, &harness.session, "ses_budget_child", "general").await;
    let tool = subagent_tool(&harness, "session_write");

    for attempt in 1..=20 {
        let result = tool
            .execute(
                &tool_context(&harness, &harness.session),
                json!({"session_id": child, "text": format!("write {attempt}")}),
            )
            .await;
        assert!(
            result.is_ok(),
            "write {attempt} must be accepted: {result:?}"
        );
    }

    let rejected = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child, "text": "one too many"}),
        )
        .await
        .expect_err("the 21st write within the window is rate limited");
    assert!(
        rejected
            .message
            .contains("rate limit reached (max 20/min); retry after "),
        "{}",
        rejected.message
    );

    let rows = settle(&harness, &child, 20).await;
    assert_eq!(rows.len(), 20, "the refused write leaves no durable row");
    assert!(
        !serde_json::to_string(&rows)
            .expect("json")
            .contains("one too many"),
        "the refused text must not be stored: {rows:?}"
    );

    // A different sender has its own window: the child may still write to
    // itself even though the parent just exhausted its budget.
    tool.execute(
        &tool_context(&harness, &child),
        json!({"session_id": child, "text": "self note"}),
    )
    .await
    .expect("another sender has an independent budget");
}

#[tokio::test]
async fn session_write_budget_honors_the_configured_limit() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"siblings": {"max_writes_per_min": 2}}}),
        None,
    )
    .await;
    let child = create_child(&harness, &harness.session, "ses_budget_cfg", "general").await;
    let tool = subagent_tool(&harness, "session_write");

    for _ in 0..2 {
        tool.execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child, "text": "ok"}),
        )
        .await
        .expect("writes under the configured limit succeed");
    }
    let rejected = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child, "text": "over"}),
        )
        .await
        .expect_err("the configured limit is honored");
    assert!(
        rejected
            .message
            .contains("rate limit reached (max 2/min); retry after "),
        "{}",
        rejected.message
    );
    assert_eq!(settle(&harness, &child, 2).await.len(), 2);
}
