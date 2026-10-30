use axum::http::{Method, StatusCode};
use oc_server::{AppState, router};
use serde_json::json;

use crate::support::{call, harness};

#[tokio::test]
async fn config_and_agent_envelopes() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(&app, Method::GET, "/api/config", None).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.unwrap();
    let entries = entries.as_array().unwrap();
    assert!(!entries.is_empty());
    assert!(entries.iter().any(|entry| entry["type"] == "document"));
    assert!(entries.iter().any(|entry| entry["path"].as_str().is_some()));

    let (status, body) = call(&app, Method::GET, "/api/config/shell", None).await;
    assert_eq!(status, StatusCode::OK);
    let shells = body.unwrap();
    let shells = shells.as_array().expect("bare ConfigShell.Option[]");
    for shell in shells {
        assert!(shell["path"].is_string());
        assert!(shell["name"].is_string());
        assert!(shell["acceptable"].is_boolean());
    }

    let (status, body) = call(&app, Method::GET, "/api/agent", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(
        body["location"]["project"]["id"]
            .as_str()
            .unwrap()
            .starts_with("prj_")
    );
    let agents = body["data"].as_array().unwrap();
    assert!(!agents.is_empty());
    let agent_id = agents[0]["id"].as_str().unwrap().to_owned();

    let (status, body) = call(&app, Method::GET, &format!("/api/agent/{agent_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_eq!(body["data"]["id"], agent_id);
    assert!(body["location"]["directory"].is_string());

    let (status, body) = call(&app, Method::GET, "/api/agent/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "AgentNotFoundError");
}

#[tokio::test]
async fn missing_sessions_are_404_everywhere() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let missing = "ses_missing_missing_missing";

    for (method, uri, body) in [
        (Method::GET, format!("/api/session/{missing}"), None),
        (
            Method::PATCH,
            format!("/api/session/{missing}"),
            Some(json!({"title": "x"})),
        ),
        (Method::DELETE, format!("/api/session/{missing}"), None),
        (
            Method::POST,
            format!("/api/session/{missing}/prompt"),
            Some(json!({"text": "x"})),
        ),
        (Method::GET, format!("/api/session/{missing}/inbox"), None),
        (
            Method::POST,
            format!("/api/session/{missing}/interrupt"),
            None,
        ),
    ] {
        let (status, body) = call(&app, method, &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError", "{uri}");
    }
}

#[tokio::test]
async fn missing_store_is_503() {
    let harness = harness().await;
    let state = AppState::new(
        harness.state.info().clone(),
        None,
        harness.state.directory().to_path_buf(),
    );
    let app = router(state);

    let (status, body) = call(&app, Method::GET, "/api/session", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/session/ses_x/prompt",
        Some(json!({"text": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");
}

#[tokio::test]
async fn model_routes_report_the_missing_catalog() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    for uri in ["/api/model", "/api/model/default", "/api/provider"] {
        let (status, body) = call(&app, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        let body = body.unwrap();
        assert_eq!(body["_tag"], "ServiceUnavailableError");
        assert_eq!(body["message"], "model catalog not loaded");
    }
}
