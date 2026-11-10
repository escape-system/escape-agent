//! Host-side compaction advice: durable, non-forcing, cooldown-gated.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_store::{MessageQuery, NewMessage, Order};
use serde_json::{Value, json};

use crate::support::*;

const ADVICE_METADATA: &str = "/metadata/advice";

async fn seed_pressure(harness: &Harness) {
    let body = "x".repeat(8_000);
    for index in 0..60 {
        harness
            .store
            .append_message(NewMessage {
                id: format!("msg_pressure_{index:02}"),
                session_id: harness.session.clone(),
                kind: "user".to_owned(),
                data: json!({"text": body}),
                time_created: oc_types::now_millis(),
            })
            .await
            .expect("pressure message");
    }
    harness
        .store
        .mark_session_busy(&harness.session, oc_types::now_millis())
        .await
        .expect("busy");
}

/// An inbox row carries the payload under `/payload`; a promoted message
/// carries the same fields at the top level.
fn advice_rows(value: &Value) -> bool {
    let advice = value
        .pointer(ADVICE_METADATA)
        .or_else(|| value.pointer(&format!("/payload{ADVICE_METADATA}")))
        .and_then(Value::as_str);
    advice == Some("compact")
}

async fn advice_rows_count(store: &oc_store::Store, session: &str) -> usize {
    let mut count = store
        .list_inbox(session)
        .await
        .expect("inbox")
        .iter()
        .filter(|row| advice_rows(row))
        .count();
    let page = store
        .list_messages(MessageQuery {
            session_id: session.to_owned(),
            limit: 200,
            order: Order::Desc,
            cursor: None,
            kind: Some("observer".to_owned()),
        })
        .await
        .expect("messages");
    count += page.items.iter().filter(|row| advice_rows(row)).count();
    count
}

async fn harness_with_observer(config: Value) -> (Harness, Arc<oc_runtime::Runtime>) {
    let harness = Harness::new(vec![], Arc::new(AllowAll), config, None).await;
    let notifier = Arc::new(RecordingNotifier::default());
    let (executor, repairs) = oc_runtime::RepairChannel::new();
    let engine = observer_engine(
        FixedObserverModel::text(&json!({"action": "none"})),
        notifier,
        executor,
    );
    let runtime = Arc::new(
        harness
            .restart()
            .with_observer(oc_runtime::ObserverBinding {
                engine,
                goals: goals(&harness),
                repairs,
            }),
    );
    runtime.start().await.expect("start");
    (harness, runtime)
}

async fn wait_for_advice(harness: &Harness, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    // The row commits before the wake and the emit, so polling the row alone
    // can observe the advice before `observer.advised` is published. Wait for
    // both, otherwise the event assertion races the advice pipeline.
    loop {
        let rowed = advice_rows_count(&harness.store, &harness.session).await > 0;
        if rowed && !harness.sink.data("observer.advised").is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for compaction advice"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn pressure_produces_one_advisory_row_and_event() {
    let (harness, runtime) = harness_with_observer(json!({})).await;
    seed_pressure(&harness).await;
    runtime.notify(&harness.session).await.expect("notify");
    wait_for_advice(&harness, Duration::from_secs(10)).await;

    let inbox = harness.inbox().await;
    assert!(
        inbox
            .iter()
            .all(|row| { row.get("type").and_then(Value::as_str) != Some("compaction") }),
        "advice must never enqueue a compaction row"
    );
    let page = harness
        .store
        .list_messages(MessageQuery {
            session_id: harness.session.clone(),
            limit: 200,
            order: Order::Desc,
            cursor: None,
            kind: Some("compaction".to_owned()),
        })
        .await
        .expect("compaction messages");
    assert!(page.items.is_empty(), "advice must not run compaction");
    assert!(!harness.sink.has("session.compaction.started"));

    let events = harness.sink.data("observer.advised");
    assert_eq!(events.len(), 1, "exactly one observer.advised event");
    assert_eq!(events[0]["sessionID"], harness.session.as_str());
    assert!(events[0]["estimateTokens"].as_u64().unwrap_or(0) > 0);
    assert!(events[0]["ceilingTokens"].as_u64().unwrap_or(0) > 0);
    assert!(events[0]["triggerTokens"].as_u64().is_some());
}

// The cooldown is proven by `observer::advice::tests::cooldown_boundary_is_exclusive`:
// it drives `decide()` with an injected clock, so deleting the cooldown fails it.
// An integration version cannot be made reliable: the advice row is delivered to
// the session actor immediately after the wake, so counting inbox rows plus
// observer messages races the delivery and can double-read or miss the row.
// `pressure_produces_one_advisory_row_and_event` still covers the end-to-end path
// (row + wake + event), and the ceiling/auto tests cover the other skips.

#[tokio::test]
async fn auto_compaction_disables_advice() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"compaction": {"auto": true}}),
        None,
    )
    .await;
    seed_pressure(&harness).await;
    let runtime = harness.restart();
    assert!(
        !runtime
            .advise_context_pressure_at(std::slice::from_ref(&harness.session), 1_000_000_000)
            .await
    );
    assert_eq!(
        advice_rows_count(&harness.store, &harness.session).await,
        0,
        "auto compaction must suppress advice"
    );
    assert!(harness.sink.data("observer.advised").is_empty());
}

#[tokio::test]
async fn an_explicit_ceiling_disables_advice() {
    // `compaction.ceiling_tokens` arms the turn loop's guard just like `auto`,
    // so advising would be redundant and the message's "no ceiling configured"
    // sentence would be false.
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"compaction": {"ceiling_tokens": 50_000}}),
        None,
    )
    .await;
    seed_pressure(&harness).await;
    let runtime = harness.restart();
    assert!(
        !runtime
            .advise_context_pressure_at(std::slice::from_ref(&harness.session), 1_000_000_000)
            .await
    );
    assert_eq!(
        advice_rows_count(&harness.store, &harness.session).await,
        0,
        "an explicit ceiling must suppress advice"
    );
    assert!(harness.sink.data("observer.advised").is_empty());
}
