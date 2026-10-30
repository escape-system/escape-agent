//! Steer and queue promotion at turn step boundaries.

use std::sync::Arc;

use oc_store::{Delivery, NewMessage};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::support::*;

#[tokio::test]
async fn steer_row_is_promoted_at_the_next_step_boundary() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: Some(Arc::clone(&gate)),
                calls: vec![call("call_read", "read", json!({"path": "hello.txt"}))],
            },
            Step::Echo {
                gate: None,
                needle: "SYNTHETIC_STEER".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start the turn").await;
    assert_eq!(
        harness.next_started().await,
        0,
        "the first step holds the gate"
    );

    let steer = harness
        .enqueue(
            "synthetic",
            "SYNTHETIC_STEER: the child finished",
            Delivery::Steer,
        )
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one provider call per step");
    assert!(
        !contains_text(&requests[0], "SYNTHETIC_STEER"),
        "the steer cannot precede its enqueue"
    );
    assert!(
        contains_text(&requests[1], "SYNTHETIC_STEER"),
        "step two must carry the promoted steer"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 2);
    assert!(
        message_text(assistants[1]).contains("mentions SYNTHETIC_STEER"),
        "the final answer references the promoted note: {}",
        message_text(assistants[1])
    );
    assert_eq!(
        of_type(&messages, "synthetic").len(),
        1,
        "the steer becomes exactly one message"
    );

    let delivered = harness.delivery_count(&steer);
    assert_eq!(delivered, 1, "one delivery event for the steer");
    assert!(harness.inbox().await.is_empty(), "the inbox drained");
}

#[tokio::test]
async fn two_steers_at_one_step_boundary_promote_in_fifo_order() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: Some(Arc::clone(&gate)),
                calls: vec![call("call_read", "read", json!({"path": "hello.txt"}))],
            },
            Step::Echo {
                gate: None,
                needle: "STEER_TWO".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start the turn").await;
    assert_eq!(harness.next_started().await, 0);

    let first = harness
        .enqueue("synthetic", "STEER_ONE: first child", Delivery::Steer)
        .await;
    let second = harness
        .enqueue("synthetic", "STEER_TWO: second child", Delivery::Steer)
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one provider call per step");
    let boundary = request_text(&requests[1]);
    let one = boundary.find("STEER_ONE").expect("first steer promoted");
    let two = boundary.find("STEER_TWO").expect("second steer promoted");
    assert!(
        one < two,
        "the boundary preserves enqueue order: {boundary}"
    );

    let messages = harness.messages().await;
    assert_eq!(
        of_type(&messages, "synthetic").len(),
        2,
        "each steer becomes exactly one message"
    );
    assert_eq!(harness.delivery_count(&first), 1, "first delivery once");
    assert_eq!(harness.delivery_count(&second), 1, "second delivery once");
    assert!(harness.inbox().await.is_empty(), "the inbox drained");
}

#[tokio::test]
async fn host_materialized_steer_doorbell_is_consumed_once() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: Some(Arc::clone(&gate)),
                calls: vec![call("call_read", "read", json!({"path": "hello.txt"}))],
            },
            Step::Echo {
                gate: None,
                needle: "HOST_STEER".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start the turn").await;
    assert_eq!(harness.next_started().await, 0);

    // The host writes the durable message and leaves the inbox row as a
    // doorbell, the same shape the prompt route uses for steer admission.
    let id = harness
        .enqueue("synthetic", "HOST_STEER: host wrote it", Delivery::Steer)
        .await;
    harness
        .store
        .append_message(NewMessage {
            id: id.clone(),
            session_id: harness.session.clone(),
            kind: "synthetic".to_owned(),
            data: json!({
                "text": "HOST_STEER: host wrote it",
                "time": {"created": oc_types::now_millis()}
            }),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("host message");
    gate.notify_one();
    harness.idle().await;

    let messages = harness.messages().await;
    let matching: Vec<&Value> = messages
        .iter()
        .filter(|message| message.get("id").and_then(Value::as_str) == Some(id.as_str()))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the doorbell must not duplicate the host message"
    );
    assert_eq!(
        of_type(&messages, "synthetic").len(),
        1,
        "the host-written synthetic is the only one"
    );
    assert_eq!(
        harness.delivery_count(&id),
        1,
        "consuming the doorbell publishes exactly one delivery event"
    );
    assert!(
        harness.inbox().await.is_empty(),
        "the doorbell was consumed"
    );
}

#[tokio::test]
async fn queue_rows_execute_in_fifo_order_after_the_turn() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Echo {
                gate: Some(Arc::clone(&gate)),
                needle: "RUNNING_TURN".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "QUEUE_ONE".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "QUEUE_TWO".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("RUNNING_TURN: start").await;
    assert_eq!(harness.next_started().await, 0);

    let first = harness
        .enqueue("user", "QUEUE_ONE: first queued", Delivery::Queue)
        .await;
    let second = harness
        .enqueue("user", "QUEUE_TWO: second queued", Delivery::Queue)
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "the running turn plus one execution per queue row"
    );
    assert!(contains_text(&requests[0], "RUNNING_TURN"));
    assert!(
        !contains_text(&requests[0], "QUEUE_ONE") && !contains_text(&requests[0], "QUEUE_TWO"),
        "queue rows never join the running turn"
    );
    assert!(
        contains_text(&requests[1], "QUEUE_ONE") && !contains_text(&requests[1], "QUEUE_TWO"),
        "the first queued execution runs first and cannot see the second row"
    );
    assert!(
        contains_text(&requests[2], "QUEUE_TWO"),
        "the second queued execution follows"
    );

    let messages = harness.messages().await;
    let users = of_type(&messages, "user");
    for needle in ["QUEUE_ONE", "QUEUE_TWO"] {
        assert_eq!(
            users
                .iter()
                .filter(|message| message_text(message).contains(needle))
                .count(),
            1,
            "{needle} becomes exactly one user message"
        );
    }
    assert_eq!(harness.delivery_count(&first), 1);
    assert_eq!(harness.delivery_count(&second), 1);
    assert!(harness.inbox().await.is_empty());
}

#[tokio::test]
async fn queue_row_waits_for_the_next_execution() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: Some(Arc::clone(&gate)),
                calls: vec![call("call_read", "read", json!({"path": "hello.txt"}))],
            },
            Step::Echo {
                gate: None,
                needle: "QUEUED_NOTE".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "QUEUED_NOTE".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start the turn").await;
    assert_eq!(harness.next_started().await, 0);

    let queued = harness
        .enqueue("user", "QUEUED_NOTE: run after this turn", Delivery::Queue)
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "two turn steps plus the queued execution"
    );
    assert!(
        !contains_text(&requests[0], "QUEUED_NOTE"),
        "a queue row is never promoted mid-turn"
    );
    assert!(
        !contains_text(&requests[1], "QUEUED_NOTE"),
        "the second step of the turn must not see the queue row"
    );
    assert!(
        contains_text(&requests[2], "QUEUED_NOTE"),
        "the next execution promotes the queue row"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 3, "two turn steps plus queued turn");
    assert!(
        message_text(assistants[1]).contains("without QUEUED_NOTE"),
        "the running turn finished without the queue row: {}",
        message_text(assistants[1])
    );
    assert!(
        message_text(assistants[2]).contains("mentions QUEUED_NOTE"),
        "the queued execution saw its own prompt"
    );
    assert_eq!(
        of_type(&messages, "user")
            .iter()
            .filter(|message| message_text(message).contains("QUEUED_NOTE"))
            .count(),
        1,
        "the queue row becomes exactly one user message"
    );
    assert_eq!(
        harness.delivery_count(&queued),
        1,
        "the queue row is delivered once, by the next execution"
    );
    assert!(harness.inbox().await.is_empty());
}

#[tokio::test]
async fn steer_arriving_during_the_final_stream_waits_for_the_next_execution() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Echo {
                gate: Some(Arc::clone(&gate)),
                needle: "LATE_STEER".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "LATE_STEER".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("single step").await;
    assert_eq!(harness.next_started().await, 0);

    // The turn has no step boundary left, so this row cannot join it.
    let late = harness
        .enqueue("synthetic", "LATE_STEER: arrived too late", Delivery::Steer)
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        2,
        "the finished turn plus one follow-up execution"
    );
    assert!(
        !contains_text(&requests[0], "LATE_STEER"),
        "the finished stream cannot see the late steer"
    );
    assert!(
        contains_text(&requests[1], "LATE_STEER"),
        "the next execution delivers the late steer"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 2);
    assert!(message_text(assistants[0]).contains("without LATE_STEER"));
    assert!(message_text(assistants[1]).contains("mentions LATE_STEER"));
    assert_eq!(harness.delivery_count(&late), 1, "delivered exactly once");
    assert!(harness.inbox().await.is_empty());
}
