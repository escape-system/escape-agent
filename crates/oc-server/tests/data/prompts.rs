use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{SseReader, call, create_session, harness, session_id, sse};

#[tokio::test]
async fn queued_prompt_stays_inbox_only_until_delivery() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({
            "id": "msg_prompt1",
            "text": "hello",
            "metadata": {"draft": true},
            "delivery": "queue",
            "files": [{"uri": "file:///tmp/a.txt"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = body.unwrap()["data"].clone();
    assert_eq!(item["id"], "msg_prompt1");
    assert_eq!(item["sessionID"], sid);
    assert_eq!(item["type"], "user");
    assert_eq!(item["delivery"], "queue");
    assert_eq!(item["payload"]["text"], "hello");
    assert_eq!(item["payload"]["metadata"]["draft"], true);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    assert_eq!(inbox["data"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["data"][0]["id"], "msg_prompt1");

    // A queued prompt is not part of the transcript until the runtime promotes
    // its inbox row; materialising it here would leak it into a running turn.
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    assert!(
        messages["data"].as_array().unwrap().is_empty(),
        "queued prompt materialised early: {messages}"
    );

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=0")).await;
    let mut reader = SseReader::new(stream);
    let mut events = Vec::new();
    while let Some(event) = reader.next_event().await {
        events.push(event);
    }
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec!["session.created", "session.inbox.enqueued", "log.synced"]
    );
    assert_eq!(events[1]["durable"]["aggregateID"], sid);
    assert_eq!(events[1]["data"]["inboxID"], "msg_prompt1");
    assert_eq!(events[1]["data"]["item"]["delivery"], "queue");
    assert!(
        events[1]["durable"]["seq"].as_i64().unwrap()
            > events[0]["durable"]["seq"].as_i64().unwrap()
    );
}

#[tokio::test]
async fn steer_prompt_materialises_message_immediately() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"id": "msg_steer1", "text": "steer me", "delivery": "steer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    let messages = messages["data"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], "msg_steer1");
    assert_eq!(messages[0]["type"], "user");
    assert_eq!(messages[0]["text"], "steer me");

    // The inbox row is only the doorbell once the message exists.
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    assert_eq!(inbox["data"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["data"][0]["delivery"], "steer");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"id": "msg_steer1", "text": "again"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["_tag"], "ConflictError");
}

#[tokio::test]
async fn duplicate_queued_prompt_id_conflicts_on_the_inbox_row() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let request = json!({"id": "msg_queued1", "text": "queued", "delivery": "queue"});
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn queued_command_is_not_materialised() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "deploy", "text": "prod", "delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    let items = inbox["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "user");
    assert_eq!(items[0]["delivery"], "queue");
    assert!(
        items[0]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("Deploy prod now.")
    );

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    assert!(
        body.unwrap()["data"].as_array().unwrap().is_empty(),
        "queued command materialised early"
    );
}

#[tokio::test]
async fn duplicate_prompt_id_conflicts() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let request = json!({"id": "msg_dupe", "text": "first"});
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"text": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "BadRequestError");
}
