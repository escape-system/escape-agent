use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::support::{Harness, call};

#[tokio::test]
async fn evaluate_creates_a_request_only_for_ask() {
    let harness = Harness::new(
        Vec::new(),
        Some(json!([
            {"action": "shell", "resource": "*", "effect": "ask"},
            {"action": "read", "resource": "*", "effect": "allow"}
        ])),
    )
    .await;

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/permission", harness.session),
        Some(json!({"action": "shell", "resources": ["echo ok"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body.unwrap()["data"].clone();
    assert_eq!(data["effect"], "ask");
    let id = data["id"].as_str().unwrap().to_owned();
    let pending = harness.pending().await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["id"], id);

    assert_eq!(harness.reply(&id, "once").await, StatusCode::NO_CONTENT);
    assert!(harness.pending().await.is_empty());

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/permission", harness.session),
        Some(json!({"action": "read", "resources": ["hello.txt"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["effect"], "allow");
    assert!(harness.pending().await.is_empty());

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn evaluate_uses_saved_always_rules() {
    let harness = Harness::new(
        Vec::new(),
        Some(json!([
            {"action": "shell", "resource": "*", "effect": "ask"}
        ])),
    )
    .await;

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/permission", harness.session),
        Some(json!({"action": "shell", "resources": ["echo saved"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body.unwrap()["data"].clone();
    assert_eq!(data["effect"], "ask");
    let id = data["id"].as_str().unwrap().to_owned();

    assert_eq!(harness.reply(&id, "always").await, StatusCode::NO_CONTENT);
    // `always` persists on a spawned task; wait for the durable rule before
    // asking again.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let saved = harness
            .store
            .list_saved_permissions(Some("prj_server_runtime"))
            .await
            .expect("saved permissions");
        if !saved.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "saved rule never landed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(harness.pending().await.is_empty());

    // The same admission request is now answered by the saved rule instead of
    // parking a second request that nothing awaits.
    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/permission", harness.session),
        Some(json!({"action": "shell", "resources": ["echo saved"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["effect"], "allow");
    assert!(harness.pending().await.is_empty());

    harness.runtime.expect("runtime").shutdown().await;
}
