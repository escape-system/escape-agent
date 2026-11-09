//! User-stop behavior: queued work is withdrawn and a running turn is
//! interrupted without waiting for the provider stream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_llm::LlmEvent;
use oc_store::{BoxQuery, Delivery, NewBox, NewInbox};
use serde_json::json;

use crate::support::*;

/// A stop that lands while a `compaction` row is still queued must withdraw the
/// row: otherwise the actor would start the compaction right after the stop.
/// Nothing else may run on the way out either — no compaction events and no
/// temporary-box expiry (that only happens on a completed compaction).
#[tokio::test]
async fn stop_withdraws_a_queued_compaction() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    harness
        .store
        .write_box(
            NewBox {
                root_id: harness.session.clone(),
                owner_session: harness.session.clone(),
                key: "scratch".to_owned(),
                title: None,
                description: Some("working notes".to_owned()),
                kind: "temporary".to_owned(),
                tags: Vec::new(),
                content: "temporary note".to_owned(),
            },
            false,
        )
        .await
        .expect("temporary box");

    harness
        .store
        .enqueue(NewInbox {
            id: oc_types::MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "compaction".to_owned(),
            payload: json!({
                "time": {"created": oc_types::now_millis()},
                "status": "pending",
                "reason": "manual"
            }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("compaction enqueued");

    // No `notify`: the row is definitely still queued when the stop lands.
    harness
        .runtime
        .interrupt(&harness.session)
        .await
        .expect("stop");
    assert!(
        harness.inbox().await.is_empty(),
        "the queued compaction row is withdrawn"
    );

    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(5))
        .await
        .expect("idle");

    assert_eq!(
        harness.sink.count("session.compaction.started"),
        0,
        "a withdrawn compaction never starts"
    );
    assert_eq!(harness.sink.count("session.compaction.ended"), 0);
    assert_eq!(harness.model.calls(), 0, "no provider call was made");

    let live = harness
        .store
        .list_boxes(BoxQuery {
            root_id: harness.session.clone(),
            kind: Some("temporary".to_owned()),
            include_expired: false,
            limit: 0,
        })
        .await
        .expect("live boxes");
    assert_eq!(
        live.items.len(),
        1,
        "a stop must not expire temporary boxes: {:?}",
        live.items
    );
}

/// The turn loop races the provider stream against the cancel token, so a stop
/// must settle well before the 30s script delay elapses.
#[tokio::test]
async fn stop_interrupts_a_slow_stream_within_two_seconds() {
    let harness = Harness::new(
        vec![Script::Delayed {
            events: vec![
                LlmEvent::StepStart,
                LlmEvent::Finish {
                    reason: "stop".to_owned(),
                    usage: usage(),
                },
            ],
            delay: Duration::from_secs(30),
        }],
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
    harness.wait_for_event("session.execution.started").await;

    let started = Instant::now();
    assert!(
        harness
            .runtime
            .interrupt(&harness.session)
            .await
            .expect("stop")
    );
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "stop must not wait for the provider stream: {elapsed:?}"
    );

    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted.len(), 1, "{interrupted:?}");
    assert_eq!(interrupted[0]["reason"], "user");
    assert_eq!(harness.sink.count("session.execution.succeeded"), 0);
}
