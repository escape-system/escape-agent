use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{SseReader, call, create_session, harness, session_id, sse};

#[tokio::test]
async fn agent_model_and_move_emit_events() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/agent"),
        Some(json!({"agent": "nope"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "AgentNotFoundError");
    assert_eq!(body["agentID"], "nope");

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/agent"),
        Some(json!({"agent": "plan"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/model"),
        Some(json!({"model": {"providerID": "test", "id": "model"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let target = tempfile::tempdir().unwrap();
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/move"),
        Some(json!({"directory": target.path().to_string_lossy()})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    let session = body.unwrap()["data"].clone();
    assert_eq!(session["agent"], "plan");
    assert_eq!(session["model"]["providerID"], "test");
    assert_eq!(session["model"]["id"], "model");

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=1")).await;
    let mut reader = SseReader::new(stream);
    let mut types = Vec::new();
    while let Some(event) = reader.next_event().await {
        if event["type"] != "log.synced" {
            types.push(event["type"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(
        types,
        vec![
            "session.agent.selected",
            "session.model.selected",
            "session.moved"
        ]
    );
}

#[tokio::test]
async fn active_interrupt_and_background_shapes() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(&app, Method::GET, "/api/session/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({"data": {}}));

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/interrupt?resume=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({"interrupted": false}));

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/background"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");
}

#[tokio::test]
async fn lane_routes_require_a_service() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(&app, Method::GET, "/api/experimental/lane", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");
}

#[tokio::test]
async fn skill_command_and_reference_catalog_routes() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(&app, Method::GET, "/api/skill", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    let skills = body["data"].as_array().unwrap();
    let demo = skills
        .iter()
        .find(|skill| skill["id"] == "demo")
        .expect("demo skill");
    assert_eq!(demo["name"], "Demo");
    assert_eq!(demo["description"], "demo skill");
    assert!(demo["content"].as_str().unwrap().contains("Do the demo"));
    assert!(demo.get("source").is_none());

    let (status, body) = call(&app, Method::GET, "/api/command", None).await;
    assert_eq!(status, StatusCode::OK);
    let commands = body.unwrap();
    let deploy = commands["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|command| command["name"] == "deploy")
        .expect("deploy command")
        .clone();
    assert_eq!(deploy["description"], "deploy things");
    assert!(deploy["template"].as_str().unwrap().contains("$ARGUMENTS"));
    assert!(deploy.get("agent").is_none());

    let (status, body) = call(&app, Method::GET, "/api/reference", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    assert!(body["data"].as_array().unwrap().is_empty());

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "deploy", "text": "prod"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    assert!(messages["data"].as_array().unwrap().iter().any(|message| {
        message["text"]
            .as_str()
            .is_some_and(|text| text.contains("Deploy prod now."))
    }));
}

#[tokio::test]
async fn compress_command_and_prompt_alias_admit_compaction() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(&app, Method::GET, "/api/command", None).await;
    assert_eq!(status, StatusCode::OK);
    let commands = body.unwrap();
    let compress = commands["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|command| command["name"] == "compress")
        .expect("builtin compress command")
        .clone();
    assert_eq!(
        compress["description"],
        "Compress the session to reduce context size"
    );
    assert!(compress.get("template").is_none());

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "compress"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"text": "/compress"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["type"], "compaction");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    let compactions = inbox["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "compaction")
        .count();
    assert_eq!(compactions, 2, "{inbox:?}");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    assert!(
        messages["data"].as_array().unwrap().iter().all(|message| {
            message["text"].as_str() != Some("/compress")
                && message["content"].as_array().is_none_or(|parts| {
                    parts
                        .iter()
                        .all(|part| part["text"].as_str() != Some("/compress"))
                })
        }),
        "the raw /compress alias must not be persisted as a user prompt: {messages:?}"
    );
}
