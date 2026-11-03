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

/// Characterization test, not a fix: `limit` is silently clamped to 200 and the
/// default page is the newest 50. Both are documented contracts
/// (`docs/02-protocol.md:114`, `docs/03-storage.md:331-333`), so a 4xx on an
/// oversized limit or a different default would be a regression, not a repair.
#[tokio::test]
async fn message_default_page_and_limit_clamp_are_unchanged() {
    // Above MAX_LIMIT (200) so the clamp is observable.
    const SEEDED: usize = 213;
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({})).await;
    let sid = session_id(&session);
    let store = harness.state.store().expect("store");
    let now = oc_types::now_millis();
    for index in 0..SEEDED {
        // The oldest and the newest row are `user`, so one page proves the
        // transcript is mixed and `type` is not filtered unless asked.
        let kind = if index == 0 || index == SEEDED - 1 {
            "user"
        } else {
            "assistant"
        };
        store
            .append_message(oc_store::NewMessage {
                id: format!("msg_p{index:03}"),
                session_id: sid.clone(),
                kind: kind.to_owned(),
                data: json!({ "text": format!("prompt {index}") }),
                time_created: now + i64::try_from(index).unwrap(),
            })
            .await
            .unwrap();
    }

    // 1. No limit: exactly 50 rows, newest first.
    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    let page = body["data"].as_array().unwrap();
    assert_eq!(page.len(), 50, "default page size is 50");
    assert_eq!(page[0]["id"], "msg_p212", "default order is desc");
    assert_eq!(
        page[49]["id"], "msg_p163",
        "the default page is the newest 50"
    );
    assert!(
        page.iter().any(|message| message["type"] == "user"),
        "the newest page must not filter by type: {body}"
    );

    // 2. limit=500 answers 200 rows, silently clamped, never a 4xx.
    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?limit=500"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the clamp is silent: {body:?}");
    let body = body.unwrap();
    let page = body["data"].as_array().unwrap();
    assert_eq!(page.len(), 200, "MAX_LIMIT clamps 500 to 200");
    assert_eq!(page[0]["id"], "msg_p212");
    assert_eq!(page[199]["id"], "msg_p013");

    // 3. asc + cursor paging reaches rows older than the default page, so a
    //    transcript that looks empty of user rows is a paging artifact.
    let mut ids: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let uri = cursor.as_ref().map_or_else(
            || format!("/api/session/{sid}/message?limit=50&order=asc"),
            |cursor| format!("/api/session/{sid}/message?limit=50&order=asc&cursor={cursor}"),
        );
        let (status, body) = call(&app, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::OK);
        let body = body.unwrap();
        let page = body["data"].as_array().unwrap();
        pages += 1;
        ids.extend(
            page.iter()
                .map(|message| message["id"].as_str().unwrap().to_owned()),
        );
        match body["cursor"]["next"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
        assert!(pages < 10, "asc paging did not terminate: {body}");
    }
    assert_eq!(pages, 5, "213 rows at 50 per page");
    assert_eq!(ids.len(), SEEDED, "every seeded row is reachable");
    assert_eq!(ids.first().map(String::as_str), Some("msg_p000"));
    assert_eq!(ids.last().map(String::as_str), Some("msg_p212"));
    assert!(
        ids.iter().any(|id| id == "msg_p000"),
        "the oldest row is older than the default page"
    );
}
