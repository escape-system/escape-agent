use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{SseReader, call, create_session, harness, session_id, sse};

#[tokio::test]
async fn log_replays_in_order_and_follows_live() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({"title": "one"})).await);

    let (status, _) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}"),
        Some(json!({"title": "two"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=0&follow=true")).await;
    let mut reader = SseReader::new(stream);

    let first = reader.next_event().await.unwrap();
    let second = reader.next_event().await.unwrap();
    let marker = reader.next_event().await.unwrap();
    assert_eq!(first["type"], "session.created");
    assert_eq!(second["type"], "session.renamed");
    assert_eq!(second["data"]["title"], "two");
    let first_seq = first["durable"]["seq"].as_i64().unwrap();
    let second_seq = second["durable"]["seq"].as_i64().unwrap();
    assert!(second_seq > first_seq);
    assert_eq!(marker["type"], "log.synced");
    assert_eq!(marker["aggregateID"], sid);
    assert_eq!(marker["seq"], second_seq);

    let (status, _) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}"),
        Some(json!({"title": "three"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let live = reader.next_event().await.unwrap();
    assert_eq!(live["type"], "session.renamed");
    assert_eq!(live["data"]["title"], "three");
    assert_eq!(live["durable"]["seq"].as_i64().unwrap(), second_seq + 1);
}

#[tokio::test]
async fn log_after_skips_replayed_events() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({"title": "one"})).await);
    call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}"),
        Some(json!({"title": "two"})),
    )
    .await;

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=1")).await;
    let mut reader = SseReader::new(stream);
    let renamed = reader.next_event().await.unwrap();
    let marker = reader.next_event().await.unwrap();
    assert_eq!(renamed["type"], "session.renamed");
    assert_eq!(renamed["durable"]["seq"], 2);
    assert_eq!(marker["seq"], 2);
    assert!(reader.next_event().await.is_none());
}

#[tokio::test]
async fn inbox_delivery_can_change_and_cancel() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (_, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"id": "msg_inbox", "text": "hold"})),
    )
    .await;
    assert_eq!(body.unwrap()["data"]["delivery"], "steer");

    let (status, _) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}/inbox/msg_inbox"),
        Some(json!({"delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    assert_eq!(body.unwrap()["data"][0]["delivery"], "queue");

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/inbox/msg_inbox"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn synthetic_compact_and_unimplemented_runtimes() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/synthetic"),
        Some(json!({"id": "msg_syn", "text": "note", "description": "why"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_eq!(body["data"]["type"], "synthetic");
    assert_eq!(body["data"]["payload"]["text"], "note");

    // A retried synthetic id that already became a durable message is a
    // conflict, not a silent no-op: the actor's doorbell branch would consume
    // the second inbox row without executing it.
    let store = harness.state.store().expect("store");
    store
        .append_message(oc_store::NewMessage {
            id: "msg_syn".to_owned(),
            session_id: sid.clone(),
            kind: "synthetic".to_owned(),
            data: json!({"text": "note"}),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("durable message");
    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/synthetic"),
        Some(json!({"id": "msg_syn", "text": "note"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/compact"),
        Some(json!({"id": "msg_cmp"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["type"], "compaction");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/shell"),
        Some(json!({"command": "ls"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "missing", "text": "y"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "CommandNotFoundError");
}
