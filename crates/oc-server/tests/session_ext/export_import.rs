use axum::http::{Method, StatusCode};
use oc_server::router;
use oc_store::NewMessage;
use oc_types::now_millis;
use serde_json::{Value, json};

use crate::support::{
    call, create_session, harness, message_ids, message_list, prompt, session_id,
};

const SECRET_KEYS: [&str; 13] = [
    "authorization",
    "apikey",
    "api_key",
    "api-key",
    "token",
    "access_token",
    "refresh_token",
    "secret",
    "client_secret",
    "password",
    "credential",
    "credentials",
    "private_key",
];

/// Asserts that no credential-bearing key survives at any depth of `value`.
fn assert_no_secret_keys(value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                assert!(
                    !SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str()),
                    "secret key {key} leaked: {value}"
                );
                assert_no_secret_keys(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_no_secret_keys(item);
            }
        }
        _ => {}
    }
}
#[tokio::test]
async fn export_import_round_trip_and_conflict() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(
        &app,
        json!({"title": "export", "metadata": {"apiKey": "sk-secret", "keep": "yes"}}),
    )
    .await;
    let sid = session_id(&session);
    prompt(&app, &sid, "one").await;
    prompt(&app, &sid, "two").await;

    let (status, exported) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/export"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let exported = exported.unwrap()["data"].clone();
    assert_eq!(exported["messages"].as_array().unwrap().len(), 2);

    let (status, sanitized) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/export?sanitize=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sanitized = sanitized.unwrap()["data"].clone();
    assert!(sanitized["info"]["metadata"].get("apiKey").is_none());
    assert_eq!(sanitized["info"]["metadata"]["keep"], "yes");

    let mut info = exported["info"].clone();
    info["id"] = json!("ses_imported_test");
    let body = json!({"info": info, "messages": exported["messages"].clone()});
    let (status, imported) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "import: {imported:?}");
    let imported = imported.unwrap()["data"].clone();
    assert_eq!(imported["id"], "ses_imported_test");
    assert_eq!(imported["title"], "export");
    let imported_ids = message_ids(&app, "ses_imported_test").await;
    assert_eq!(imported_ids.len(), 2);
    // `session_message.id` is database-wide unique, so an import next to its
    // source must not reuse the exported ids.
    let source_ids: Vec<&str> = exported["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["id"].as_str())
        .collect();
    assert!(
        imported_ids
            .iter()
            .all(|id| !source_ids.contains(&id.as_str()))
    );

    let (status, conflict) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict.unwrap()["_tag"], "ConflictError");

    let (status, missing) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(json!({
            "info": {"id": "ses_child_test", "parentID": "ses_absent"},
            "messages": [],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn sanitize_strips_nested_secret_keys_without_mutating_the_store() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(
        &app,
        json!({
            "title": "secrets",
            "metadata": {
                "apiKey": "sk-live",
                "nested": {"authorization": "Bearer x", "keep": 1},
                "list": [{"token": "t", "ok": 2}],
            },
        }),
    )
    .await;
    let sid = session_id(&session);
    harness
        .state
        .store()
        .unwrap()
        .append_message(NewMessage {
            id: "msg_secret_user".to_owned(),
            session_id: sid.clone(),
            kind: "user".to_owned(),
            data: json!({"text": "hi", "apiKey": "sk-msg", "private_key": "pem"}),
            time_created: now_millis(),
        })
        .await
        .unwrap();

    let (status, sanitized) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/export?sanitize=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = sanitized.unwrap()["data"].clone();
    assert_no_secret_keys(&data);
    assert_eq!(data["info"]["metadata"]["nested"]["keep"], 1);
    assert_eq!(data["info"]["metadata"]["list"][0]["ok"], 2);
    let messages = data["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        1,
        "only the appended message belongs to the export"
    );
    assert_eq!(messages[0]["text"], "hi");

    let (status, raw) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/export"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let raw = raw.unwrap()["data"].clone();
    assert_eq!(raw["info"]["metadata"]["apiKey"], "sk-live");
    assert_eq!(
        raw["info"]["metadata"]["nested"]["authorization"],
        "Bearer x"
    );
    assert_eq!(raw["messages"][0]["private_key"], "pem");

    let (status, stored) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let stored = stored.unwrap()["data"].clone();
    assert_eq!(stored["metadata"]["apiKey"], "sk-live");
    assert_eq!(stored["metadata"]["nested"]["authorization"], "Bearer x");
    assert_eq!(stored["metadata"]["list"][0]["token"], "t");
    let stored_messages = message_list(&app, &sid).await;
    assert_eq!(stored_messages[0]["apiKey"], "sk-msg");
    assert_eq!(stored_messages[0]["private_key"], "pem");
}

/// An import that fails after `create_session` must not leave the session
/// behind.
///
/// The failure is reached without any injection seam: `import.rs` remaps every
/// message id to a fresh one, so an id used by another session cannot collide.
/// Two messages sharing one id inside the *same* body can: both map to the same
/// fresh identity, the first append commits, and the second one hits the
/// database-wide `session_message.id` primary key (`append_message` only checks
/// duplicates inside the target session). The handler then rolls the session
/// back; the failure it returns is the append error.
#[tokio::test]
async fn failed_import_removes_the_partially_created_session() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let (status, response) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(json!({
            "info": {"id": "ses_half_imported", "title": "half"},
            "messages": [
                {"id": "msg_import_dupe", "type": "user", "text": "first"},
                {"id": "msg_import_dupe", "type": "user", "text": "second"},
            ],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{response:?}");
    assert_eq!(response.unwrap()["_tag"], "ConflictError");

    let (status, missing) = call(&app, Method::GET, "/api/session/ses_half_imported", None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a failed import must roll back its session: {missing:?}"
    );
    assert_eq!(missing.unwrap()["_tag"], "SessionNotFoundError");

    // Neither the session nor its first message may survive in a list.
    let (status, listed) = call(&app, Method::GET, "/api/session", None).await;
    assert_eq!(status, StatusCode::OK);
    let listed = listed.unwrap();
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|session| session["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"ses_half_imported"),
        "rolled back session still listed: {ids:?}"
    );
}

#[tokio::test]
async fn import_rejects_a_message_without_type_before_creating_the_session() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let (status, response) = call(
        &app,
        Method::POST,
        "/api/experimental/session/import",
        Some(json!({
            "info": {"id": "ses_untyped"},
            "messages": [{"text": "x"}],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response:?}");
    let response = response.unwrap();
    assert_eq!(response["_tag"], "InvalidRequestError");
    assert_eq!(response["message"], "each message needs a type");

    let (status, missing) = call(&app, Method::GET, "/api/session/ses_untyped", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let missing = missing.unwrap();
    assert_eq!(missing["_tag"], "SessionNotFoundError");
    assert_eq!(missing["sessionID"], "ses_untyped");
}
