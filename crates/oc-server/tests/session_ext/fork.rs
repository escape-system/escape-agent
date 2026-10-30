use axum::http::{Method, StatusCode};
use oc_server::router;
use oc_store::NewMessage;
use oc_types::now_millis;
use serde_json::json;

use crate::support::{
    call, create_session, harness, message_ids, message_list, prompt, session_id, take_events,
};

#[tokio::test]
async fn fork_copies_history_before_boundary() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(
        &app,
        json!({"title": "parent", "agent": "build", "metadata": {"k": "v"}}),
    )
    .await;
    let sid = session_id(&session);
    prompt(&app, &sid, "first").await;
    let second = prompt(&app, &sid, "second").await;
    let mut events = harness.state.bus().subscribe();

    let (status, forked) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/fork"),
        Some(json!({"before": second})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fork: {forked:?}");
    let forked = forked.unwrap()["data"].clone();
    let child = session_id(&forked);
    assert_ne!(child, sid);
    assert!(forked.get("parentID").is_none() || forked["parentID"].is_null());
    assert_eq!(forked["title"], "parent");
    assert_eq!(forked["agent"], "build");
    assert_eq!(forked["metadata"], json!({"k": "v"}));
    assert_eq!(
        forked["location"]["directory"],
        session["location"]["directory"]
    );

    let messages = message_ids(&app, &child).await;
    assert_eq!(
        messages.len(),
        1,
        "only the boundary-excluded prefix copies"
    );
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{child}/message?order=asc"),
        None,
    )
    .await;
    assert_eq!(body.unwrap()["data"][0]["text"], "first");

    let published = take_events(&mut events, "session.forked");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({
            "sessionID": child,
            "parentID": sid,
            "boundary": { "type": "before", "messageID": second },
        })
    );
    assert!(
        published[0].data.get("instructions").is_none(),
        "optional instructions are omitted, never null"
    );
    let durable = published[0].durable.as_ref().expect("fork is durable");
    assert_eq!(durable.version, 2);
    assert_eq!(durable.aggregate_id, child);
    assert!(durable.seq > 0, "the fork event is persisted with a seq");
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (status, missing) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/fork"),
        Some(json!({"before": "msg_absent"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "MessageNotFoundError");
    assert!(
        take_events(&mut events, "session.forked").is_empty(),
        "a rejected fork must not publish"
    );

    // Without `before` the whole history copies and the boundary is `through`.
    let (status, forked) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/fork"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "through fork: {forked:?}");
    let through_child = session_id(&forked.unwrap()["data"]);
    assert_eq!(message_ids(&app, &through_child).await.len(), 2);
    let published = take_events(&mut events, "session.forked");
    assert_eq!(published.len(), 1);
    assert_eq!(
        published[0].data,
        json!({
            "sessionID": through_child,
            "parentID": sid,
            "boundary": { "type": "through", "messageID": second },
        })
    );
    assert_eq!(published[0].durable.as_ref().unwrap().version, 2);
}

#[allow(clippy::too_many_lines)] // one linear fork-then-import scenario; splitting hides the copied shapes
#[tokio::test]
async fn fork_and_import_remap_compaction_keep_id_at_the_top_level() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let store = harness.state.store().unwrap().clone();

    // Fork route: the runtime writes `keepID` at the top level of the row.
    let parent = create_session(&app, json!({"title": "keepID fork"})).await;
    let parent_id = session_id(&parent);
    store
        .append_message(NewMessage {
            id: "msg_fork_user".to_owned(),
            session_id: parent_id.clone(),
            kind: "user".to_owned(),
            data: json!({"text": "kept"}),
            time_created: now_millis(),
        })
        .await
        .unwrap();
    store
        .append_message(NewMessage {
            id: "msg_fork_compaction".to_owned(),
            session_id: parent_id.clone(),
            kind: "compaction".to_owned(),
            data: json!({"keepID": "msg_fork_user", "summary": "s", "status": "completed"}),
            time_created: now_millis(),
        })
        .await
        .unwrap();
    let (status, forked) = call(
        &app,
        Method::POST,
        &format!("/api/session/{parent_id}/fork"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fork: {forked:?}");
    let child = session_id(&forked.unwrap()["data"]);
    let forked_messages = message_list(&app, &child).await;
    assert_eq!(forked_messages.len(), 2);
    assert_eq!(forked_messages[0]["type"], "user");
    assert_eq!(forked_messages[1]["type"], "compaction");
    let forked_user = forked_messages[0]["id"].as_str().unwrap();
    assert_ne!(forked_user, "msg_fork_user", "fork assigns fresh ids");
    assert_eq!(forked_messages[1]["keepID"], forked_user);
    assert!(
        forked_messages[1].pointer("/metadata/keepID").is_none(),
        "the remapped reference lives at the top level only: {}",
        forked_messages[1]
    );

    // Import route: same top-level shape.
    let (status, imported) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(json!({
            "info": {"id": "ses_import_keep"},
            "messages": [
                {"type": "user", "id": "msg_import_user", "text": "kept"},
                {
                    "type": "compaction",
                    "id": "msg_import_compaction",
                    "keepID": "msg_import_user",
                    "summary": "s",
                    "status": "completed",
                },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "import: {imported:?}");
    let imported_messages = message_list(&app, "ses_import_keep").await;
    assert_eq!(imported_messages.len(), 2);
    assert_eq!(imported_messages[0]["type"], "user");
    let imported_user = imported_messages[0]["id"].as_str().unwrap();
    assert_ne!(imported_user, "msg_import_user", "import assigns fresh ids");
    assert_eq!(imported_messages[1]["keepID"], imported_user);
    assert!(
        imported_messages[1].pointer("/metadata/keepID").is_none(),
        "the remapped reference lives at the top level only: {}",
        imported_messages[1]
    );

    // Legacy rows that nested the reference under `metadata` still remap, and
    // the stale nested key is dropped.
    let (status, imported) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(json!({
            "info": {"id": "ses_import_legacy"},
            "messages": [
                {"type": "user", "id": "msg_legacy_user", "text": "kept"},
                {
                    "type": "compaction",
                    "id": "msg_legacy_compaction",
                    "metadata": {"keepID": "msg_legacy_user", "other": true},
                    "summary": "s",
                },
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "legacy import: {imported:?}");
    let legacy_messages = message_list(&app, "ses_import_legacy").await;
    assert_eq!(legacy_messages.len(), 2);
    let legacy_user = legacy_messages[0]["id"].as_str().unwrap();
    assert_ne!(legacy_user, "msg_legacy_user");
    assert_eq!(legacy_messages[1]["keepID"], legacy_user);
    assert!(legacy_messages[1].pointer("/metadata/keepID").is_none());
    assert_eq!(legacy_messages[1]["metadata"]["other"], true);
}
