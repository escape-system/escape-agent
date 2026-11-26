//! Delivery of messages that were materialised at admission time.
//!
//! `Store::enqueue_materialized` (used by `talk` and `session_write`) commits the
//! inbox doorbell **and** the transcript row in one transaction, so the message
//! is readable before any turn serves it, carrying `metadata.inboxID` and
//! `metadata.delivered = false`.
//!
//! The runtime used to treat *any* pre-existing message row as a pure doorbell:
//! it deleted the inbox row with `consume_inbox` and never flipped
//! `metadata.delivered`. The message was then permanently indistinguishable from
//! an undelivered one, and the delivery event could not repair it afterwards
//! because `project_inbox_delivered` looks the doorbell up by id and the row was
//! already gone. These tests pin the contract that matters to the model: a
//! materialised, undelivered row is *delivered* (marker flipped, exactly one
//! turn, text reaches the request) and a second drain is a no-op.

use std::sync::Arc;

use oc_store::{Delivery, MessageQuery, NewInbox, NewSession, NewSessionTree, Order, Store};
use oc_types::MessageId;
use serde_json::{Value, json};

use crate::support::*;

/// Every transcript row whose `metadata.inboxID` is `inbox_id`.
async fn rows_for(store: &Store, session: &str, inbox_id: &str) -> Vec<Value> {
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
        for message in page.items {
            if message.pointer("/metadata/inboxID").and_then(Value::as_str) == Some(inbox_id) {
                out.push(message);
            }
        }
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

fn delivered(message: &Value) -> Option<bool> {
    message
        .pointer("/metadata/delivered")
        .and_then(Value::as_bool)
}

/// Asserts the post-turn transcript shape for one materialised inbox id.
///
/// Exactly one row, keyed by the admission-time id, flipped to delivered, with
/// no doorbell left behind.
async fn assert_delivered_once(harness: &Harness, session: &str, inbox_id: &str) {
    let after = rows_for(&harness.store, session, inbox_id).await;
    assert_eq!(after.len(), 1, "no duplicate row: {after:?}");
    assert_eq!(
        after[0]["id"].as_str(),
        Some(inbox_id),
        "the admission-time row keeps its id"
    );
    assert_eq!(
        delivered(&after[0]),
        Some(true),
        "delivery flips the marker in place: {:?}",
        after[0]
    );
    assert!(
        harness
            .store
            .list_inbox(session)
            .await
            .expect("inbox")
            .is_empty(),
        "the doorbell is retired"
    );
}

/// A materialised peer row queued to an idle session must still run exactly one
/// turn, flip its delivery marker, and reach the model.
#[tokio::test]
async fn a_materialised_peer_row_is_delivered_not_swallowed_as_a_doorbell() {
    let harness = Harness::new(
        vec![text_script("acknowledged")],
        Arc::new(AllowAll),
        json!({"subagents": {"siblings": {"enabled": true}}}),
        None,
    )
    .await;
    let child = "ses_materialised_delivery".to_owned();
    create_child(&harness, &harness.session, &child, "general").await;

    // Admit a peer message the way `session_write`/`talk` do: the doorbell and
    // the transcript row commit together, `delivered` starts false.
    let inbox_id = MessageId::new().into_inner();
    let peer_text = "peer note that must reach the model";
    harness
        .store
        .enqueue_materialized(&NewInbox {
            id: inbox_id.clone(),
            session_id: child.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({
                "text": peer_text,
                "kind": "note",
                "from": harness.session,
                "agent": "general",
                "metadata": {
                    "source": "peer",
                    "from": harness.session,
                    "agent": "general",
                    "kind": "note",
                },
            }),
            delivery: Delivery::Steer,
        })
        .await
        .expect("materialised peer row");

    // Visible in history before delivery, and explicitly not yet delivered.
    let before = rows_for(&harness.store, &child, &inbox_id).await;
    assert_eq!(before.len(), 1, "one row at admission: {before:?}");
    assert_eq!(
        delivered(&before[0]),
        Some(false),
        "admission marks the row undelivered: {:?}",
        before[0]
    );
    assert!(
        harness.model.requests().is_empty(),
        "no request before the target is woken"
    );

    harness.runtime.notify(&child).await.expect("notify child");
    harness
        .runtime
        .wait_idle(&child, IDLE_WAIT)
        .await
        .expect("child idle");

    // The turn ran and the model actually received the peer text.
    let requests = harness.model.requests();
    assert!(
        !requests.is_empty(),
        "a materialised peer row must run a turn, not be swallowed; events: {:#?}",
        harness.sink.kinds()
    );
    assert!(
        requests
            .iter()
            .any(|request| request_contains(request, peer_text)),
        "the peer text must reach the model: {:#?}",
        requests.len()
    );

    // Exactly one row, same id, now delivered, and no doorbell left behind.
    assert_delivered_once(&harness, &child, &inbox_id).await;
    assert_eq!(
        harness.delivery_count(&inbox_id),
        0,
        "delivery_count only counts this harness session"
    );

    // A second drain has nothing left to do: the doorbell is gone, so no
    // duplicate turn is possible for the same id.
    let requests_after_first = requests.len();
    harness.runtime.notify(&child).await.expect("notify again");
    harness
        .runtime
        .wait_idle(&child, IDLE_WAIT)
        .await
        .expect("child idle again");
    assert_eq!(
        harness.model.requests().len(),
        requests_after_first,
        "a second drain must not run a second turn for the same id"
    );
    assert_eq!(
        rows_for(&harness.store, &child, &inbox_id).await.len(),
        1,
        "and must not duplicate the row"
    );
}

/// The legitimate doorbell path is unchanged: a host that materialised its own
/// message keeps one row, gets a turn, and is never repainted as cancelled.
#[tokio::test]
async fn a_host_materialised_message_keeps_the_doorbell_path() {
    let harness = Harness::new(
        vec![text_script("answered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let inbox_id = MessageId::new().into_inner();
    let text = "host written prompt";

    // The host writes the message itself, then rings the doorbell.
    harness
        .store
        .append_message(oc_store::NewMessage {
            id: inbox_id.clone(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            data: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("host message");
    harness
        .store
        .enqueue(NewInbox {
            id: inbox_id.clone(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("doorbell");

    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("wake");
    harness
        .runtime
        .wait_idle(&harness.session, IDLE_WAIT)
        .await
        .expect("idle");

    assert_eq!(
        harness.model.requests().len(),
        1,
        "a host-materialised doorbell still runs its turn"
    );
    assert!(
        harness
            .model
            .requests()
            .iter()
            .any(|request| request_contains(request, text))
    );
    let rows: Vec<Value> = harness
        .store
        .get_message(&harness.session, &inbox_id)
        .await
        .expect("read back")
        .into_iter()
        .collect();
    assert_eq!(rows.len(), 1, "no duplicate row: {rows:?}");
    assert!(
        harness
            .store
            .list_inbox(&harness.session)
            .await
            .expect("inbox")
            .is_empty(),
        "the doorbell is consumed"
    );
}

/// A queued (not merely steered) materialised row reaches an idle target too,
/// which is the `session_write` `delivery: "queue"` path.
#[tokio::test]
async fn a_queued_materialised_row_wakes_an_idle_target() {
    let harness = Harness::new(
        vec![text_script("queued and served")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let inbox_id = MessageId::new().into_inner();
    let text = "queued peer question";
    harness
        .store
        .enqueue_materialized(&NewInbox {
            id: inbox_id.clone(),
            session_id: harness.session.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({
                "text": text,
                "kind": "question",
                "from": "ses_parent",
                "agent": "general",
                "metadata": {
                    "source": "peer",
                    "from": "ses_parent",
                    "agent": "general",
                    "kind": "question",
                },
            }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("queued row");

    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("wake");
    harness
        .runtime
        .wait_idle(&harness.session, IDLE_WAIT)
        .await
        .expect("idle");

    assert!(
        harness
            .model
            .requests()
            .iter()
            .any(|request| request_contains(request, text)),
        "a queued materialised row must reach the model"
    );
    let rows = rows_for(&harness.store, &harness.session, &inbox_id).await;
    assert_eq!(rows.len(), 1, "no duplicate row: {rows:?}");
    assert_eq!(
        delivered(&rows[0]),
        Some(true),
        "marker flipped: {:?}",
        rows[0]
    );
}

/// A fresh session with a tree row, for the child-side assertions above.
#[allow(dead_code)]
async fn unrelated_session(store: &Store, id: &str) {
    store
        .create_session_with_tree(
            NewSession {
                id: id.to_owned(),
                parent_id: None,
                project_id: "prj_materialised".to_owned(),
                directory: "/tmp".to_owned(),
                title: Some("unrelated".to_owned()),
                agent: Some("engineer".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: id.to_owned(),
                depth: 0,
                role: "primary".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("session");
}
