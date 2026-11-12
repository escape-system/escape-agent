use axum::http::{Method, StatusCode};
use oc_server::router;
use oc_types::now_millis;
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

#[tokio::test]
async fn prompt_normalizes_data_url_file_attachments() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({
            "id": "msg_img_prompt",
            "text": "look at this image",
            "delivery": "steer",
            "files": [{
                "uri": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
                "name": "pixel.png"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = body.unwrap()["data"].clone();
    assert_eq!(item["id"], "msg_img_prompt");

    // Check materialised message row in transcript
    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let messages = body.unwrap();
    let user_msg = messages["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "msg_img_prompt")
        .expect("user message found");

    let files = user_msg["files"].as_array().expect("files array present");
    assert_eq!(files.len(), 1);
    let file = &files[0];
    assert_eq!(file["name"], "pixel.png");
    assert_eq!(file["mime"], "image/png");
    assert_eq!(file["source"]["type"], "inline");
    assert_eq!(
        file["data"],
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
    );

    // Verify it deserializes cleanly into oc_types::message::UserMessage
    let parsed: oc_types::message::UserMessage =
        serde_json::from_value(user_msg.clone()).expect("UserMessage deserializes");
    assert_eq!(parsed.id.into_inner(), "msg_img_prompt");
    assert_eq!(parsed.files.as_ref().unwrap().len(), 1);
    let f = &parsed.files.as_ref().unwrap()[0];
    assert_eq!(f.mime, "image/png");
    assert_eq!(f.name.as_deref(), Some("pixel.png"));
}

/// A steer prompt whose message append fails must not leave its inbox row
/// behind: the runtime promotes surviving `steer` rows on the next turn, so a
/// leftover row would run text the caller was told had failed.
///
/// The append is failed without an injection seam. `session_message.id` is a
/// database-wide primary key while `admit` only pre-checks the *target*
/// session, so a message id that already belongs to another session passes the
/// pre-check, allows the enqueue to commit, and then fails the insert.
///
/// HONEST LIMIT: `cancel_inbox` routes through the same single serialized
/// writer as every other write, so it cannot be made to fail from outside. The
/// retry and exhausted-retry arms are therefore untestable here and are
/// reported only; this test pins the invariant on the arm that is reachable.
#[tokio::test]
async fn failed_steer_append_removes_the_inbox_row() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let owner = session_id(&create_session(&app, json!({"title": "owner"})).await);
    let target = session_id(&create_session(&app, json!({"title": "target"})).await);

    // Seed a message identity that belongs to `owner` only. It has no inbox
    // row, so the enqueue below is free to allocate the same id.
    harness
        .state
        .store()
        .unwrap()
        .append_message(oc_store::NewMessage {
            id: "msg_t7_collision".to_owned(),
            session_id: owner.clone(),
            kind: "user".to_owned(),
            data: json!({"text": "seed"}),
            time_created: now_millis(),
        })
        .await
        .unwrap();

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{target}/prompt"),
        Some(json!({
            "id": "msg_t7_collision",
            "text": "this must not run",
            "delivery": "steer",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body:?}");
    assert_eq!(
        body.unwrap()["_tag"],
        "InternalError",
        "the append failure is the error returned to the caller"
    );

    // The text must not be deliverable: neither as a pending inbox row nor as a
    // committed transcript message.
    let (_, inbox) = call(
        &app,
        Method::GET,
        &format!("/api/session/{target}/inbox"),
        None,
    )
    .await;
    let inbox = inbox.unwrap();
    assert!(
        inbox["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != "msg_t7_collision"),
        "failed steer prompt left its inbox row behind: {inbox}"
    );
    let (_, messages) = call(
        &app,
        Method::GET,
        &format!("/api/session/{target}/message?order=asc"),
        None,
    )
    .await;
    assert!(messages.unwrap()["data"].as_array().unwrap().is_empty());

    // The owner's message is untouched: the failed append wrote nothing.
    let (_, owner_messages) = call(
        &app,
        Method::GET,
        &format!("/api/session/{owner}/message?order=asc"),
        None,
    )
    .await;
    let owner_messages = owner_messages.unwrap();
    assert_eq!(owner_messages["data"].as_array().unwrap().len(), 1);
    assert_eq!(owner_messages["data"][0]["text"], "seed");
}
