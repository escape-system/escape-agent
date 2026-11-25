//! Peer-message materialization: an inbox row and its transcript message are
//! committed together, promotion flips the delivery marker instead of inserting
//! a duplicate, and cancellation keeps a peer row with a cancelled marker.
//!
//! Every test uses its own in-memory database; no network and no real keys.

use oc_store::{
    Delivery, MessageQuery, NewInbox, NewMessage, NewSession, Order, Store, StoreError,
};
use serde_json::{Value, json};

fn new_session(id: &str) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: None,
        project_id: "prj_1".to_owned(),
        directory: "/tmp/one".to_owned(),
        title: Some(format!("title {id}")),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        time_created: 1_000,
    }
}

async fn seed_session(store: &Store, id: &str) -> String {
    store
        .create_session(new_session(id))
        .await
        .expect("create session");
    id.to_owned()
}

fn peer_inbox(session: &str, id: &str, delivery: Delivery) -> NewInbox {
    NewInbox {
        id: id.to_owned(),
        session_id: session.to_owned(),
        kind: "synthetic".to_owned(),
        payload: json!({
            "text": "hello from a sibling",
            "from": "ses_sender",
            "agent": "general",
            "kind": "question",
            "metadata": { "source": "peer", "from": "ses_sender" },
        }),
        delivery,
    }
}

async fn messages(store: &Store, session: &str) -> Vec<Value> {
    store
        .list_messages(MessageQuery {
            limit: 100,
            order: Order::Asc,
            ..MessageQuery::new(session)
        })
        .await
        .expect("list messages")
        .items
}

#[tokio::test]
async fn enqueue_materialized_writes_inbox_and_message_in_one_tx() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_materialize").await;

    let (row, message_id) = store
        .enqueue_materialized(&peer_inbox(&session, "msg_materialize", Delivery::Queue))
        .await
        .expect("enqueue materialized");

    assert_eq!(row.id, "msg_materialize");
    assert_eq!(row.session_id, session);
    assert_eq!(row.kind, "synthetic");
    assert_eq!(row.delivery, Delivery::Queue);
    assert!(row.time_created > 0);
    assert_eq!(row.message_id(), message_id);
    assert_eq!(message_id.as_str(), "msg_materialize");

    // The wire row keeps the enqueue-time `time.created` the message carries.
    let wire = row.to_wire();
    assert_eq!(wire["id"], "msg_materialize");
    assert_eq!(wire["sessionID"], session);
    assert_eq!(wire["type"], "synthetic");
    assert_eq!(wire["delivery"], "queue");
    assert_eq!(wire["time"]["created"], row.time_created);
    assert_eq!(wire["payload"]["metadata"]["source"], "peer");

    // The message is visible through the transcript reader immediately.
    let listed = messages(&store, &session).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    let message = &listed[0];
    assert_eq!(message["id"], "msg_materialize");
    assert_eq!(message["type"], "synthetic");
    assert_eq!(message["text"], "hello from a sibling");
    assert_eq!(message["time"]["created"], row.time_created);
    assert_eq!(message["metadata"]["inboxID"], "msg_materialize");
    assert_eq!(message["metadata"]["delivered"], false);
    assert_eq!(message["metadata"]["source"], "peer");
    assert!(message.get("deliveredAt").is_none());

    // The doorbell row is pending.
    let pending = store.list_inbox(&session).await.expect("list inbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], "msg_materialize");

    // No `null` leaks into the wire body.
    assert!(!message.to_string().contains("null"));
}

#[tokio::test]
async fn promotion_does_not_duplicate_materialized_message() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_promote_materialized").await;

    store
        .enqueue_materialized(&peer_inbox(&session, "msg_promoted", Delivery::Steer))
        .await
        .expect("enqueue materialized");
    let before = messages(&store, &session).await;
    assert_eq!(before.len(), 1);
    let seq_before = store
        .get_message(&session, "msg_promoted")
        .await
        .expect("get")
        .expect("materialised");
    assert_eq!(seq_before["metadata"]["delivered"], false);

    let promoted = store
        .promote_inbox(&session, "msg_promoted")
        .await
        .expect("promote")
        .expect("promoted message");
    assert_eq!(promoted["id"], "msg_promoted");
    assert_eq!(promoted["metadata"]["delivered"], true);
    assert!(promoted["metadata"]["deliveredAt"].as_i64().unwrap() > 0);

    // Exactly one row, same id and `seq`: promotion must not re-materialise.
    let after = messages(&store, &session).await;
    assert_eq!(after.len(), 1, "promotion inserted a duplicate: {after:?}");
    assert_eq!(after[0]["id"], "msg_promoted");
    assert_eq!(after[0]["time"]["created"], before[0]["time"]["created"]);
    assert_eq!(after[0]["metadata"]["inboxID"], "msg_promoted");

    // The row keeps its enqueue-time position relative to a later message.
    store
        .append_message(NewMessage {
            id: "msg_after".to_owned(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            data: json!({ "text": "later" }),
            time_created: 2_000,
        })
        .await
        .expect("append later");
    let ordered = messages(&store, &session).await;
    assert_eq!(
        ordered
            .iter()
            .map(|item| item["id"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["msg_promoted", "msg_after"]
    );

    // The inbox row is consumed and a second promotion is a no-op.
    assert!(store.list_inbox(&session).await.expect("inbox").is_empty());
    assert!(
        store
            .promote_inbox(&session, "msg_promoted")
            .await
            .expect("promote again")
            .is_none()
    );
    assert_eq!(messages(&store, &session).await.len(), 2);
}

#[tokio::test]
async fn cancel_keeps_peer_message_with_cancelled_marker() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_cancel_peer").await;

    store
        .enqueue_materialized(&peer_inbox(&session, "msg_cancelled", Delivery::Queue))
        .await
        .expect("enqueue materialized");

    assert!(
        store
            .cancel_inbox(&session, "msg_cancelled")
            .await
            .expect("cancel")
    );
    assert!(store.list_inbox(&session).await.expect("inbox").is_empty());

    let listed = messages(&store, &session).await;
    assert_eq!(listed.len(), 1, "cancel deleted peer history: {listed:?}");
    assert_eq!(listed[0]["id"], "msg_cancelled");
    assert_eq!(listed[0]["metadata"]["cancelled"], true);
    assert!(listed[0]["metadata"]["cancelledAt"].as_i64().unwrap() > 0);
    assert_eq!(listed[0]["metadata"]["delivered"], false);

    // A second cancel has nothing to consume.
    assert!(
        !store
            .cancel_inbox(&session, "msg_cancelled")
            .await
            .expect("cancel again")
    );
    assert_eq!(messages(&store, &session).await.len(), 1);
}

#[tokio::test]
async fn mark_peer_cancelled_marks_only_peer_rows() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_mark_peer").await;

    store
        .enqueue_materialized(&peer_inbox(&session, "msg_peer_mark", Delivery::Steer))
        .await
        .expect("enqueue peer");

    // Not a peer row: the marker must not apply and the row must survive.
    store
        .append_message(NewMessage {
            id: "msg_user_mark".to_owned(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            data: json!({ "text": "mine" }),
            time_created: 1_500,
        })
        .await
        .expect("append user");
    assert!(
        !store
            .mark_peer_cancelled(&session, "msg_user_mark")
            .await
            .expect("mark user")
    );
    assert!(
        store
            .get_message(&session, "msg_user_mark")
            .await
            .expect("get")
            .expect("user row")
            .get("metadata")
            .is_none()
    );

    assert!(
        store
            .mark_peer_cancelled(&session, "msg_peer_mark")
            .await
            .expect("mark peer")
    );
    let peer = store
        .get_message(&session, "msg_peer_mark")
        .await
        .expect("get")
        .expect("peer row");
    assert_eq!(peer["metadata"]["cancelled"], true);
    assert!(
        !store
            .mark_peer_cancelled(&session, "msg_peer_mark")
            .await
            .expect("mark twice"),
        "an already cancelled row is not rewritten"
    );
}

#[tokio::test]
async fn legacy_inbox_row_still_inserts_on_promotion() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_legacy_promote").await;

    // A pre-existing pending row (no materialised message, no `inboxID`
    // anchor) must keep the original insert-at-promotion behaviour.
    store
        .enqueue(NewInbox {
            id: "msg_legacy_user".to_owned(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({ "text": "queued prompt" }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("enqueue legacy");
    assert!(
        store
            .get_message(&session, "msg_legacy_user")
            .await
            .expect("get")
            .is_none()
    );
    assert!(messages(&store, &session).await.is_empty());

    let promoted = store
        .promote_inbox(&session, "msg_legacy_user")
        .await
        .expect("promote")
        .expect("promoted");
    assert_eq!(promoted["id"], "msg_legacy_user");
    assert_eq!(promoted["type"], "user");
    assert_eq!(promoted["text"], "queued prompt");
    // The legacy path was unchanged: no delivery bookkeeping appears.
    assert!(promoted.get("metadata").is_none());

    let listed = messages(&store, &session).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "msg_legacy_user");
    assert_eq!(listed[0]["type"], "user");
    assert_eq!(listed[0]["metadata"], Value::Null);
}

#[tokio::test]
async fn enqueue_materialized_rolls_back_both_rows_on_a_message_id_collision() {
    let store = Store::open_memory().await.expect("memory store");
    let owner = seed_session(&store, "ses_materialize_owner").await;
    let target = seed_session(&store, "ses_materialize_target").await;

    // `session_message.id` is a database-wide primary key while the inbox
    // duplicate check only looks at `session_inbox`, so the message insert can
    // fail after the inbox insert succeeded. The transaction must then leave no
    // executable inbox row and no partial message behind.
    store
        .append_message(NewMessage {
            id: "msg_materialize_collision".to_owned(),
            session_id: owner.clone(),
            kind: "user".to_owned(),
            data: json!({ "text": "seed" }),
            time_created: 1_000,
        })
        .await
        .expect("seed owner message");

    let error = store
        .enqueue_materialized(&peer_inbox(
            &target,
            "msg_materialize_collision",
            Delivery::Queue,
        ))
        .await
        .expect_err("cross-session id collision must fail");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "collision surfaces the primary-key failure, got {error:?}"
    );

    assert!(
        store
            .list_inbox(&target)
            .await
            .expect("target inbox")
            .is_empty(),
        "failed admission left an executable inbox row"
    );
    assert!(
        store
            .get_message(&target, "msg_materialize_collision")
            .await
            .expect("target message")
            .is_none()
    );
    assert_eq!(
        store
            .get_message(&owner, "msg_materialize_collision")
            .await
            .expect("owner message")
            .expect("owner row")["text"],
        "seed"
    );
}

#[tokio::test]
async fn enqueue_materialized_rejects_control_kinds_and_bad_payloads() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_materialize_reject").await;

    for kind in ["compaction", "move"] {
        let error = store
            .enqueue_materialized(&NewInbox {
                id: format!("msg_{kind}_reject"),
                session_id: session.clone(),
                kind: kind.to_owned(),
                payload: json!({}),
                delivery: Delivery::Queue,
            })
            .await
            .expect_err("control kinds have no transcript message");
        assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");
    }

    let error = store
        .enqueue_materialized(&NewInbox {
            id: "msg_bad_payload".to_owned(),
            session_id: session.clone(),
            kind: "synthetic".to_owned(),
            payload: json!("not an object"),
            delivery: Delivery::Queue,
        })
        .await
        .expect_err("payload must be an object");
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");

    let error = store
        .enqueue_materialized(&NewInbox {
            id: "msg_bad_metadata".to_owned(),
            session_id: session.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({ "text": "x", "metadata": "not an object" }),
            delivery: Delivery::Queue,
        })
        .await
        .expect_err("metadata must be an object");
    assert!(matches!(error, StoreError::Invalid(_)), "{error:?}");

    assert!(store.list_inbox(&session).await.expect("inbox").is_empty());
    assert!(messages(&store, &session).await.is_empty());
}

#[tokio::test]
async fn delivered_event_relay_flips_the_materialized_row_without_duplicating() {
    use oc_store::NewEvent;

    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_delivered_event").await;

    store
        .enqueue_materialized(&peer_inbox(
            &session,
            "msg_event_delivered",
            Delivery::Steer,
        ))
        .await
        .expect("enqueue materialized");

    // Replaying the durable delivery (the server/runtime publishes it after a
    // promote) must update the existing row in place.
    store
        .append_event(NewEvent {
            id: "evt_inbox_delivered".to_owned(),
            aggregate_id: session.clone(),
            kind: "session.inbox.delivered".to_owned(),
            data: json!({ "sessionID": session, "inboxID": "msg_event_delivered" }),
            durable: true,
            version: 1,
            created: 9_000,
        })
        .await
        .expect("delivered event");

    let listed = messages(&store, &session).await;
    assert_eq!(listed.len(), 1, "delivery replayed a duplicate: {listed:?}");
    assert_eq!(listed[0]["id"], "msg_event_delivered");
    assert_eq!(listed[0]["metadata"]["delivered"], true);
    assert!(store.list_inbox(&session).await.expect("inbox").is_empty());
}

#[tokio::test]
async fn cancelled_event_relay_marks_peer_without_duplicating() {
    use oc_store::NewEvent;

    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_cancelled_event").await;

    store
        .enqueue_materialized(&peer_inbox(
            &session,
            "msg_event_cancelled",
            Delivery::Queue,
        ))
        .await
        .expect("enqueue materialized");

    store
        .append_event(NewEvent {
            id: "evt_inbox_cancelled".to_owned(),
            aggregate_id: session.clone(),
            kind: "session.inbox.cancelled".to_owned(),
            data: json!({ "sessionID": session, "inboxID": "msg_event_cancelled" }),
            durable: true,
            version: 1,
            created: 9_100,
        })
        .await
        .expect("cancelled event");

    let listed = messages(&store, &session).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["metadata"]["cancelled"], true);
    assert!(store.list_inbox(&session).await.expect("inbox").is_empty());
}

#[tokio::test]
async fn consume_inbox_retires_a_doorbell_without_marking_it_cancelled() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_consume").await;

    store
        .enqueue_materialized(&peer_inbox(&session, "msg_consumed", Delivery::Steer))
        .await
        .expect("enqueue materialized");
    assert!(
        store
            .consume_inbox(&session, "msg_consumed")
            .await
            .expect("consume")
    );
    assert!(store.list_inbox(&session).await.expect("inbox").is_empty());

    let message = store
        .get_message(&session, "msg_consumed")
        .await
        .expect("get")
        .expect("message kept");
    assert!(
        message["metadata"].get("cancelled").is_none(),
        "delivery must not look cancelled: {message}"
    );
    assert_eq!(message["metadata"]["delivered"], false);
}
