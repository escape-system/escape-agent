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

/// The depth prefix is found by offset in the original string, so a multi-byte
/// separator before it can never be cut through (it used to panic the handler).
#[tokio::test]
async fn subagent_depth_prefix_accepts_unicode_separators() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    for (text, depth, remainder) in [("\u{00a0}7 run", 7, "run"), ("\u{3000}3 abc", 3, "abc")] {
        let (status, body) = call(
            &app,
            Method::POST,
            &format!("/api/session/{sid}/command"),
            Some(json!({"name": "complex", "text": text})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{text:?}: {body:?}");

        let (_, body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
        let session = body.unwrap()["data"].clone();
        assert_eq!(
            session["metadata"]["maxDepth"], depth,
            "{text:?}: {session:?}"
        );
        assert_eq!(session["metadata"]["subagentMode"], "complex", "{text:?}");

        let (_, body) = call(
            &app,
            Method::GET,
            &format!("/api/session/{sid}/message?order=asc"),
            None,
        )
        .await;
        let messages = body.unwrap();
        let texts: Vec<&str> = messages["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["type"] == "user")
            .filter_map(|message| message["text"].as_str())
            .collect();
        assert_eq!(
            texts.last().copied(),
            Some(remainder),
            "{text:?}: {messages:?}"
        );
    }
}

/// Leading whitespace before the depth was counted as if it were the depth
/// word, so `"  7 run"` parsed depth 7 and still forwarded `"7 run"`.
#[tokio::test]
async fn subagent_depth_prefix_skips_leading_whitespace() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "complex", "text": "  7 run"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let (_, body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(body.unwrap()["data"]["metadata"]["maxDepth"], 7);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    let texts: Vec<&str> = messages["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["type"] == "user")
        .filter_map(|message| message["text"].as_str())
        .collect();
    assert_eq!(texts, vec!["run"], "{messages:?}");
}

/// The `/mode` prompt path and the `/command` path share one parser, so they
/// must agree on the depth prefix and leave user spacing alone.
#[tokio::test]
async fn mode_slash_command_keeps_user_spacing() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"text": "/basic hello   world"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["payload"]["text"], "hello   world");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"text": "/basic\u{3000}3 abc"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["payload"]["text"], "abc");

    let (_, body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(body.unwrap()["data"]["metadata"]["maxDepth"], 3);
}
