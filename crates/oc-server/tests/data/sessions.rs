use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{call, create_session, harness, session_id};

#[tokio::test]
async fn create_list_get_patch_delete() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let created = create_session(
        &app,
        json!({"title": "first", "metadata": {"a": 1}, "agent": "build"}),
    )
    .await;
    let id = session_id(&created);
    assert!(id.starts_with("ses_"));
    assert_eq!(created["title"], "first");
    assert_eq!(created["agent"], "build");
    assert!(created["projectID"].as_str().unwrap().starts_with("prj_"));
    assert!(created["location"]["directory"].is_string());

    let (status, body) = call(&app, Method::GET, &format!("/api/session/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["id"], id);

    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{id}"),
        Some(json!({"title": "renamed", "archived": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (_, body) = call(&app, Method::GET, &format!("/api/session/{id}"), None).await;
    let session = body.unwrap()["data"].clone();
    assert_eq!(session["title"], "renamed");
    assert!(session["time"]["archived"].is_i64());

    let (status, body) = call(&app, Method::GET, "/api/session", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_eq!(body["data"][0]["id"], id);
    assert!(body["cursor"].is_object());

    let (status, _) = call(&app, Method::DELETE, &format!("/api/session/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(&app, Method::GET, &format!("/api/session/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn parent_filter_selects_roots_and_children() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let root = create_session(&app, json!({"title": "root"})).await;
    let root_id = session_id(&root);
    let child = create_session(&app, json!({"title": "child", "parentID": root_id})).await;
    assert_eq!(child["parentID"], root_id);

    let (_, body) = call(&app, Method::GET, "/api/session?parentID=null", None).await;
    let body = body.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], root_id);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session?parentID={root_id}"),
        None,
    )
    .await;
    let body = body.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], child["id"]);
}

#[tokio::test]
async fn message_pagination_and_lookup() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({})).await;
    let sid = session_id(&session);

    for (index, id) in ["msg_a", "msg_b", "msg_c"].iter().enumerate() {
        let (status, _) = call(
            &app,
            Method::POST,
            &format!("/api/session/{sid}/prompt"),
            Some(json!({"id": id, "text": format!("prompt {index}")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?limit=2&order=asc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    let first_page = body["data"].as_array().unwrap();
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page[0]["id"], "msg_a");
    assert_eq!(first_page[0]["type"], "user");
    let next = body["cursor"]["next"].as_str().unwrap().to_owned();

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?limit=2&order=asc&cursor={next}"),
        None,
    )
    .await;
    let body = body.unwrap();
    let second_page = body["data"].as_array().unwrap();
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0]["id"], "msg_c");

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message/msg_b"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["text"], "prompt 1");

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message/msg_missing"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "MessageNotFoundError");
}
