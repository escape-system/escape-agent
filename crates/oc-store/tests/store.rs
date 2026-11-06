//! Integration tests for the SQLite store. Every test uses its own database
//! (in memory or under a temporary directory); no network and no real keys.

#![allow(clippy::too_many_lines)] // scenario tests read top-to-bottom on purpose

use oc_store::{
    Delivery, MessageQuery, NewEvent, NewInbox, NewMessage, NewSession, Order, SessionPatch,
    SessionQuery, Store, StoreError,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn new_session(id: &str, project: &str, directory: &str, created: i64) -> NewSession {
    NewSession {
        id: id.to_owned(),
        parent_id: None,
        project_id: project.to_owned(),
        directory: directory.to_owned(),
        title: Some(format!("title {id}")),
        agent: None,
        model: None,
        metadata: None,
        permissions: None,
        time_created: created,
    }
}

fn child_session(id: &str, parent: &str, created: i64) -> NewSession {
    NewSession {
        parent_id: Some(parent.to_owned()),
        ..new_session(id, "prj_1", "/tmp/one", created)
    }
}

fn new_message(session: &str, id: &str, kind: &str, text: &str, created: i64) -> NewMessage {
    NewMessage {
        id: id.to_owned(),
        session_id: session.to_owned(),
        kind: kind.to_owned(),
        data: json!({ "text": text }),
        time_created: created,
    }
}

fn ids(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .map(|value| value["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

async fn seed_session(store: &Store, id: &str) -> String {
    store
        .create_session(new_session(id, "prj_1", "/tmp/one", 1_000))
        .await
        .expect("create session");
    id.to_owned()
}

#[tokio::test]
async fn file_store_lifecycle_and_reopen() {
    let directory = TempDir::new().expect("temp dir");
    let path = directory.path().join("nested").join("opencode.db");

    let store = Store::open(&path).await.expect("open file store");
    assert_eq!(store.path(), Some(path.as_path()));
    let created = store
        .create_session(new_session("ses_a", "prj_1", "/tmp/a", 1_000))
        .await
        .expect("create");
    assert_eq!(created["id"], "ses_a");
    assert_eq!(created["projectID"], "prj_1");
    assert_eq!(created["location"]["directory"], "/tmp/a");
    assert_eq!(created["time"]["created"], 1_000);
    store
        .append_message(new_message("ses_a", "msg_a", "user", "hello", 1_100))
        .await
        .expect("append");
    drop(store);

    assert!(path.exists(), "database file is created");
    let reopened = Store::open(&path).await.expect("reopen");
    let session = reopened
        .get_session("ses_a")
        .await
        .expect("get")
        .expect("session present after reopen");
    assert_eq!(session["title"], "title ses_a");
    let message = reopened
        .get_message("ses_a", "msg_a")
        .await
        .expect("get message")
        .expect("message present after reopen");
    assert_eq!(message["text"], "hello");
    assert_eq!(message["sessionID"], "ses_a");
}

#[tokio::test]
async fn session_crud_filters_and_pagination() {
    let store = Store::open_memory().await.expect("memory store");
    store
        .create_session(new_session("ses_parent", "prj_1", "/tmp/one", 1_000))
        .await
        .expect("parent");
    store
        .create_session(child_session("ses_child", "ses_parent", 2_000))
        .await
        .expect("child");
    store
        .create_session(new_session("ses_other", "prj_2", "/tmp/two", 3_000))
        .await
        .expect("other");

    let all = store
        .list_sessions(SessionQuery {
            limit: 10,
            ..Default::default()
        })
        .await
        .expect("list all");
    assert_eq!(
        ids(&all.items),
        vec!["ses_other", "ses_child", "ses_parent"]
    );
    assert_eq!(all.items[1]["parentID"], "ses_parent");

    let roots = store
        .list_sessions(SessionQuery {
            parent_id: Some(None),
            ..Default::default()
        })
        .await
        .expect("list roots");
    assert_eq!(ids(&roots.items), vec!["ses_other", "ses_parent"]);

    let children = store
        .list_sessions(SessionQuery {
            parent_id: Some(Some("ses_parent".to_owned())),
            ..Default::default()
        })
        .await
        .expect("list children");
    assert_eq!(ids(&children.items), vec!["ses_child"]);

    let by_directory = store
        .list_sessions(SessionQuery {
            directory: Some("/tmp/one".to_owned()),
            ..Default::default()
        })
        .await
        .expect("list directory");
    assert_eq!(ids(&by_directory.items), vec!["ses_child", "ses_parent"]);

    let by_project = store
        .list_sessions(SessionQuery {
            project: Some("prj_1".to_owned()),
            ..Default::default()
        })
        .await
        .expect("list project");
    assert_eq!(ids(&by_project.items), vec!["ses_child", "ses_parent"]);

    let searched = store
        .list_sessions(SessionQuery {
            search: Some("child".to_owned()),
            ..Default::default()
        })
        .await
        .expect("search");
    assert_eq!(ids(&searched.items), vec!["ses_child"]);

    let by_subpath = store
        .list_sessions(SessionQuery {
            subpath: Some("crates".to_owned()),
            ..Default::default()
        })
        .await
        .expect("list subpath");
    assert_eq!(by_subpath.items.len(), 0);

    // Ascending page of 2 plus the cursor chain (before any patch moves time_updated).
    let first = store
        .list_sessions(SessionQuery {
            limit: 2,
            order: Order::Asc,
            ..Default::default()
        })
        .await
        .expect("asc page");
    assert_eq!(ids(&first.items), vec!["ses_parent", "ses_child"]);
    assert!(first.cursor.previous.is_none());
    let next = first.cursor.next.expect("next cursor");
    let second = store
        .list_sessions(SessionQuery {
            cursor: Some(next),
            ..Default::default()
        })
        .await
        .expect("next page");
    assert_eq!(ids(&second.items), vec!["ses_other"]);
    assert!(second.cursor.previous.is_some());
    let back = store
        .list_sessions(SessionQuery {
            cursor: second.cursor.previous,
            ..Default::default()
        })
        .await
        .expect("previous page");
    assert_eq!(ids(&back.items), vec!["ses_parent", "ses_child"]);

    store
        .patch_session(
            "ses_parent",
            SessionPatch {
                title: Some("renamed parent".to_owned()),
                ..Default::default()
            },
        )
        .await
        .expect("patch title");
    let renamed = store
        .get_session("ses_parent")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(renamed["title"], "renamed parent");

    store
        .patch_session(
            "ses_parent",
            SessionPatch {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("archive");
    let archived = store
        .list_sessions(SessionQuery {
            archived: Some(true),
            ..Default::default()
        })
        .await
        .expect("list archived");
    assert_eq!(ids(&archived.items), vec!["ses_parent"]);
    let unarchived = store
        .list_sessions(SessionQuery {
            archived: Some(false),
            ..Default::default()
        })
        .await
        .expect("list unarchived");
    assert_eq!(ids(&unarchived.items), vec!["ses_other", "ses_child"]);

    let invalid = store
        .list_sessions(SessionQuery {
            cursor: Some("not-base64!!".to_owned()),
            ..Default::default()
        })
        .await;
    assert!(matches!(invalid, Err(StoreError::InvalidCursor(_))));
    let wrong_shape = store
        .list_sessions(SessionQuery {
            cursor: Some("e30".to_owned()),
            ..Default::default()
        })
        .await;
    assert!(matches!(wrong_shape, Err(StoreError::InvalidCursor(_))));

    store.delete_session("ses_parent").await.expect("delete");
    assert!(
        store
            .get_session("ses_parent")
            .await
            .expect("get")
            .is_none()
    );
    assert!(store.get_session("ses_child").await.expect("get").is_none());
    assert!(store.get_session("ses_other").await.expect("get").is_some());

    let missing = store
        .patch_session(
            "ses_missing",
            SessionPatch {
                title: Some("nope".to_owned()),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(missing, Err(StoreError::NotFound(_))));
}

#[tokio::test]
async fn message_pagination_both_directions() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_messages").await;
    for index in 0..5 {
        let kind = if index % 2 == 0 { "user" } else { "assistant" };
        store
            .append_message(new_message(
                &session,
                &format!("msg_{index}"),
                kind,
                &format!("body {index}"),
                1_000 + index,
            ))
            .await
            .expect("append message");
    }

    let first = store
        .list_messages(MessageQuery {
            limit: 2,
            ..MessageQuery::new(&session)
        })
        .await
        .expect("desc page");
    assert_eq!(ids(&first.items), vec!["msg_4", "msg_3"]);
    assert_eq!(first.items[0]["text"], "body 4");
    assert_eq!(first.items[0]["type"], "user");
    assert_eq!(first.items[1]["type"], "assistant");
    assert!(first.cursor.previous.is_none());
    let next = first.cursor.next.expect("next");

    let second = store
        .list_messages(MessageQuery {
            limit: 2,
            cursor: Some(next),
            ..MessageQuery::new(&session)
        })
        .await
        .expect("second page");
    assert_eq!(ids(&second.items), vec!["msg_2", "msg_1"]);
    assert!(second.cursor.previous.is_some());
    assert!(second.cursor.next.is_some());

    let previous = store
        .list_messages(MessageQuery {
            limit: 2,
            cursor: second.cursor.previous.clone(),
            ..MessageQuery::new(&session)
        })
        .await
        .expect("previous page");
    assert_eq!(ids(&previous.items), vec!["msg_4", "msg_3"]);

    let tail = store
        .list_messages(MessageQuery {
            limit: 2,
            cursor: second.cursor.next.clone(),
            ..MessageQuery::new(&session)
        })
        .await
        .expect("tail page");
    assert_eq!(ids(&tail.items), vec!["msg_0"]);
    assert!(tail.cursor.next.is_none());
    assert!(tail.cursor.previous.is_some());

    let ascending = store
        .list_messages(MessageQuery {
            limit: 5,
            order: Order::Asc,
            ..MessageQuery::new(&session)
        })
        .await
        .expect("asc page");
    assert_eq!(
        ids(&ascending.items),
        vec!["msg_0", "msg_1", "msg_2", "msg_3", "msg_4"]
    );

    let users = store
        .list_messages(MessageQuery {
            kind: Some("user".to_owned()),
            ..MessageQuery::new(&session)
        })
        .await
        .expect("kind filter");
    assert_eq!(ids(&users.items), vec!["msg_4", "msg_2", "msg_0"]);

    let invalid = store
        .list_messages(MessageQuery {
            cursor: Some("e30".to_owned()),
            ..MessageQuery::new(&session)
        })
        .await;
    assert!(matches!(invalid, Err(StoreError::InvalidCursor(_))));

    let missing_anchor = store
        .list_messages(MessageQuery {
            cursor: Some(oc_store_cursor_hack("msg_missing")),
            ..MessageQuery::new(&session)
        })
        .await
        .expect("missing anchor page");
    assert!(missing_anchor.items.is_empty());

    assert!(
        store
            .get_message(&session, "msg_nope")
            .await
            .expect("get")
            .is_none()
    );
    let updated = store
        .update_message(&session, "msg_0", json!({ "text": "edited" }))
        .await;
    assert!(updated.is_ok());
    let edited = store
        .get_message(&session, "msg_0")
        .await
        .expect("get")
        .expect("present");
    assert_eq!(edited["text"], "edited");
    let missing = store
        .update_message(&session, "msg_nope", json!({ "text": "x" }))
        .await;
    assert!(matches!(missing, Err(StoreError::NotFound(_))));
}

/// Builds a syntactically valid cursor pointing at a message that does not exist.
fn oc_store_cursor_hack(message_id: &str) -> String {
    use base64::Engine as _;
    let payload = format!(r#"{{"id":"{message_id}","order":"desc","direction":"next"}}"#);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
}

#[tokio::test]
async fn inbox_enqueue_promote_cancel_and_delivery() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_inbox").await;

    let queued = store
        .enqueue(NewInbox {
            id: "msg_inbox_user".to_owned(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({ "text": "hello" }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("enqueue user");
    assert_eq!(queued["delivery"], "queue");
    assert_eq!(queued["payload"]["text"], "hello");
    assert_eq!(queued["type"], "user");

    store
        .enqueue(NewInbox {
            id: "msg_inbox_synthetic".to_owned(),
            session_id: session.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({ "text": "notice" }),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueue synthetic");

    let duplicate = store
        .enqueue(NewInbox {
            id: "msg_inbox_user".to_owned(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({ "text": "again" }),
            delivery: Delivery::Queue,
        })
        .await;
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));

    let pending = store.list_inbox(&session).await.expect("list inbox");
    assert_eq!(ids(&pending), vec!["msg_inbox_user", "msg_inbox_synthetic"]);

    store
        .set_delivery(&session, "msg_inbox_user", Delivery::Steer)
        .await
        .expect("set delivery");
    let pending = store.list_inbox(&session).await.expect("list inbox");
    assert_eq!(pending[0]["delivery"], "steer");

    let promoted = store
        .promote_inbox(&session, "msg_inbox_user")
        .await
        .expect("promote")
        .expect("promoted message");
    assert_eq!(promoted["id"], "msg_inbox_user");
    assert_eq!(promoted["type"], "user");
    assert_eq!(promoted["text"], "hello");
    assert_eq!(promoted["sessionID"], session);
    assert!(
        store
            .get_message(&session, "msg_inbox_user")
            .await
            .expect("get message")
            .is_some()
    );

    let pending = store.list_inbox(&session).await.expect("list inbox");
    assert_eq!(ids(&pending), vec!["msg_inbox_synthetic"]);
    assert!(
        store
            .promote_inbox(&session, "msg_inbox_synthetic")
            .await
            .expect("promote")
            .is_some()
    );
    assert!(store.list_inbox(&session).await.expect("list").is_empty());
    assert!(
        store
            .promote_inbox(&session, "msg_inbox_user")
            .await
            .expect("promote again")
            .is_none()
    );

    assert!(
        !store
            .cancel_inbox(&session, "msg_inbox_user")
            .await
            .expect("cancel absent")
    );

    store
        .enqueue(NewInbox {
            id: "msg_inbox_move".to_owned(),
            session_id: session.clone(),
            kind: "move".to_owned(),
            payload: json!({ "location": { "directory": "/tmp/two" } }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("enqueue move");
    assert!(
        store
            .promote_inbox(&session, "msg_inbox_move")
            .await
            .expect("promote control")
            .is_none()
    );
    assert_eq!(store.list_inbox(&session).await.expect("list").len(), 1);
    assert!(
        store
            .cancel_inbox(&session, "msg_inbox_move")
            .await
            .expect("cancel move")
    );

    let unknown = store
        .set_delivery(&session, "msg_inbox_missing", Delivery::Queue)
        .await;
    assert!(matches!(unknown, Err(StoreError::NotFound(_))));

    let bad_kind = store
        .enqueue(NewInbox {
            id: "msg_inbox_bad".to_owned(),
            session_id: session.clone(),
            kind: "bogus".to_owned(),
            payload: json!({ "text": "x" }),
            delivery: Delivery::Queue,
        })
        .await;
    assert!(matches!(bad_kind, Err(StoreError::Invalid(_))));
}

#[tokio::test]
async fn sequence_is_monotonic_and_dense() {
    let store = Store::open_memory().await.expect("memory store");
    let session = seed_session(&store, "ses_seq").await;

    let first = store
        .append_event(NewEvent {
            id: "evt_1".to_owned(),
            aggregate_id: session.clone(),
            kind: "test.ping".to_owned(),
            data: json!({ "n": 1 }),
            durable: true,
            version: 1,
            created: 10,
        })
        .await
        .expect("event one");
    let second = store
        .append_event(NewEvent {
            id: "evt_2".to_owned(),
            aggregate_id: session.clone(),
            kind: "test.ping".to_owned(),
            data: json!({ "n": 2 }),
            durable: true,
            version: 2,
            created: 11,
        })
        .await
        .expect("event two");
    assert_eq!(first, 1);
    assert_eq!(second, 2);

    store
        .append_message(new_message(
            &session,
            "msg_seq",
            "user",
            "consumes a seq",
            12,
        ))
        .await
        .expect("message");
    let third = store
        .append_event(NewEvent {
            id: "evt_3".to_owned(),
            aggregate_id: session.clone(),
            kind: "test.ping".to_owned(),
            data: json!({ "n": 3 }),
            durable: true,
            version: 1,
            created: 13,
        })
        .await
        .expect("event three");
    assert_eq!(third, second + 2, "message reserved a sequence number");

    let ephemeral = store
        .append_event(NewEvent {
            id: "evt_ephemeral".to_owned(),
            aggregate_id: session.clone(),
            kind: "session.text.delta".to_owned(),
            data: json!({ "sessionID": session }),
            durable: false,
            version: 1,
            created: 14,
        })
        .await
        .expect("ephemeral");
    assert_eq!(ephemeral, 0);

    let before = store
        .get_session(&session)
        .await
        .expect("get")
        .expect("present");
    store
        .append_event(NewEvent {
            id: "evt_unknown".to_owned(),
            aggregate_id: session.clone(),
            kind: "test.unknown".to_owned(),
            data: json!({ "sessionID": session, "payload": true }),
            durable: true,
            version: 3,
            created: 99,
        })
        .await
        .expect("unknown event");
    let after = store
        .get_session(&session)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(before["time"]["updated"], after["time"]["updated"]);

    let events = store.events_after(&session, 0, 100).await.expect("events");
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![1, 2, 4, 5]
    );
    assert_eq!(events[1].version, 2);
    assert_eq!(events[1].kind, "test.ping");
    assert_eq!(events[1].data["n"], 2);

    let tail = store
        .events_after(&session, 2, 100)
        .await
        .expect("events after");
    assert_eq!(
        tail.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![4, 5]
    );

    let duplicate = store
        .append_event(NewEvent {
            id: "evt_1".to_owned(),
            aggregate_id: session.clone(),
            kind: "test.ping".to_owned(),
            data: json!({}),
            durable: true,
            version: 1,
            created: 20,
        })
        .await;
    assert!(matches!(duplicate, Err(StoreError::Conflict(_))));

    let empty_kind = store
        .append_event(NewEvent {
            id: "evt_empty".to_owned(),
            aggregate_id: session.clone(),
            kind: String::new(),
            data: json!({}),
            durable: true,
            version: 1,
            created: 21,
        })
        .await;
    assert!(matches!(empty_kind, Err(StoreError::Invalid(_))));
}

#[tokio::test]
async fn delete_cascades_messages_inbox_children_and_events() {
    let store = Store::open_memory().await.expect("memory store");
    store
        .create_session(new_session("ses_root", "prj_1", "/tmp/one", 1_000))
        .await
        .expect("root");
    store
        .create_session(child_session("ses_kid", "ses_root", 2_000))
        .await
        .expect("kid");
    store
        .append_message(new_message("ses_kid", "msg_kid", "user", "hi", 2_100))
        .await
        .expect("message");
    store
        .enqueue(NewInbox {
            id: "msg_pending".to_owned(),
            session_id: "ses_root".to_owned(),
            kind: "user".to_owned(),
            payload: json!({ "text": "pending" }),
            delivery: Delivery::Queue,
        })
        .await
        .expect("inbox");
    store
        .append_event(NewEvent {
            id: "evt_kid".to_owned(),
            aggregate_id: "ses_kid".to_owned(),
            kind: "test.ping".to_owned(),
            data: json!({}),
            durable: true,
            version: 1,
            created: 2_200,
        })
        .await
        .expect("event");

    store.delete_session("ses_root").await.expect("delete");

    assert!(store.get_session("ses_root").await.expect("get").is_none());
    assert!(store.get_session("ses_kid").await.expect("get").is_none());
    assert!(
        store
            .list_messages(MessageQuery::new("ses_kid"))
            .await
            .expect("messages")
            .items
            .is_empty()
    );
    assert!(
        store
            .list_inbox("ses_root")
            .await
            .expect("inbox")
            .is_empty()
    );
    assert!(
        store
            .events_after("ses_kid", 0, 100)
            .await
            .expect("events")
            .is_empty()
    );

    // Deleting an unknown session is a no-op.
    store
        .delete_session("ses_absent")
        .await
        .expect("idempotent");
}

#[tokio::test]
async fn event_projection_rebuilds_rows() {
    let store = Store::open_memory().await.expect("memory store");
    let session = "ses_projected";

    store
        .append_event(NewEvent {
            id: "evt_created".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.created".to_owned(),
            data: json!({
                "sessionID": session,
                "projectID": "prj_projected",
                "location": { "directory": "/tmp/projected" },
                "subpath": "src",
                "title": "projected",
                "agent": "build",
                "metadata": { "source": "event" },
                "permissions": [],
            }),
            durable: true,
            version: 1,
            created: 5_000,
        })
        .await
        .expect("created event");
    let created = store
        .get_session(session)
        .await
        .expect("get")
        .expect("session projected from event");
    assert_eq!(created["title"], "projected");
    assert_eq!(created["agent"], "build");
    assert_eq!(created["location"]["directory"], "/tmp/projected");
    assert_eq!(created["subpath"], "src");

    store
        .append_event(NewEvent {
            id: "evt_enqueued".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.inbox.enqueued".to_owned(),
            data: json!({
                "sessionID": session,
                "inboxID": "msg_projected",
                "item": {
                    "type": "user",
                    "payload": { "text": "promote me" },
                    "delivery": "steer",
                },
            }),
            durable: true,
            version: 1,
            created: 5_100,
        })
        .await
        .expect("enqueued event");
    let pending = store.list_inbox(session).await.expect("inbox");
    assert_eq!(ids(&pending), vec!["msg_projected"]);

    store
        .append_event(NewEvent {
            id: "evt_delivered".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.inbox.delivered".to_owned(),
            data: json!({ "sessionID": session, "inboxID": "msg_projected" }),
            durable: true,
            version: 1,
            created: 5_200,
        })
        .await
        .expect("delivered event");
    assert!(store.list_inbox(session).await.expect("inbox").is_empty());
    let message = store
        .get_message(session, "msg_projected")
        .await
        .expect("get")
        .expect("message projected");
    assert_eq!(message["type"], "user");
    assert_eq!(message["text"], "promote me");
    assert_eq!(message["time"]["created"], 5_200);

    store
        .append_event(NewEvent {
            id: "evt_renamed".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.renamed".to_owned(),
            data: json!({ "sessionID": session, "title": "renamed" }),
            durable: true,
            version: 1,
            created: 5_300,
        })
        .await
        .expect("renamed event");
    let renamed = store
        .get_session(session)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(renamed["title"], "renamed");

    store
        .append_event(NewEvent {
            id: "evt_synthetic".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.synthetic".to_owned(),
            data: json!({ "sessionID": session, "text": "notice", "description": "note" }),
            durable: true,
            version: 1,
            created: 5_400,
        })
        .await
        .expect("synthetic event");
    let synthetic = store
        .get_message(session, "msg_synthetic")
        .await
        .expect("get")
        .expect("synthetic message projected");
    assert_eq!(synthetic["type"], "synthetic");
    assert_eq!(synthetic["text"], "notice");

    store
        .append_event(NewEvent {
            id: "evt_archived".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.archived".to_owned(),
            data: json!({ "sessionID": session }),
            durable: true,
            version: 1,
            created: 5_500,
        })
        .await
        .expect("archived event");
    let archived = store
        .list_sessions(SessionQuery {
            archived: Some(true),
            ..Default::default()
        })
        .await
        .expect("archived sessions");
    assert_eq!(ids(&archived.items), vec![session]);

    store
        .append_event(NewEvent {
            id: "evt_deleted".to_owned(),
            aggregate_id: session.to_owned(),
            kind: "session.deleted".to_owned(),
            data: json!({ "sessionID": session }),
            durable: true,
            version: 2,
            created: 5_600,
        })
        .await
        .expect("deleted event");
    assert!(store.get_session(session).await.expect("get").is_none());
    let events = store.events_after(session, 0, 100).await.expect("events");
    assert_eq!(
        events.len(),
        7,
        "the log survives the session.deleted event"
    );
    assert_eq!(events[6].kind, "session.deleted");
    assert_eq!(events[6].version, 2);
}

#[tokio::test]
async fn a_database_is_owned_by_one_server_at_a_time() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("owned.sqlite");
    let store = Store::open(&path).await.expect("first open");
    let error = Store::open(&path).await.expect_err("second open must fail");
    match error {
        StoreError::Conflict(message) => {
            assert!(
                message.contains("already in use"),
                "unexpected conflict message: {message}"
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    drop(store);
    let reopened = Store::open(&path).await.expect("reopen after drop");
    drop(reopened);
}

#[tokio::test]
async fn active_child_count_propagates_a_sql_failure() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("count.sqlite");
    let store = Store::open(&path).await.expect("open store");
    store
        .create_session(new_session("ses_parent", "prj_1", "/tmp/one", 1_000))
        .await
        .expect("parent");
    assert_eq!(
        store.active_child_count("ses_parent").await.expect("count"),
        0,
        "a session without children counts zero"
    );

    // A second connection to the same file drops the table the count reads.
    // The store's read pool then hits a real `no such table` from SQLite.
    {
        let attacker = rusqlite::Connection::open(&path).expect("second connection");
        attacker
            .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE session_v2;")
            .expect("drop the table the count reads");
    }

    let error = store
        .active_child_count("ses_parent")
        .await
        .expect_err("a missing table must not read as zero children");
    assert!(
        matches!(error, StoreError::Sqlite(_)),
        "expected a propagated sqlite error, got {error:?}"
    );
}
