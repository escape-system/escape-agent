use axum::http::{Method, StatusCode};
use oc_llm::LlmEvent;
use serde_json::json;

use crate::support::{Harness, Script, call, wait_until_active};

#[tokio::test]
async fn background_detaches_running_and_idle_sessions() {
    let harness = Harness::new(vec![Script::HangAfter(vec![LlmEvent::StepStart])], None).await;

    // Idle: there is nothing to detach, but the route still succeeds.
    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/background", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (status, _) = harness.prompt("hang").await;
    assert_eq!(status, StatusCode::OK);
    wait_until_active(&harness).await;

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/background", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    // Detaching never cancels the execution: the actor keeps running.
    let (_, body) = call(&harness.app, Method::GET, "/api/session/active", None).await;
    assert!(
        body.expect("active body")["data"]
            .get(&harness.session)
            .is_some(),
        "the backgrounded execution is still running"
    );

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/interrupt", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({ "interrupted": true }));
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn absent_runtime_still_admits_and_reports_unavailable() {
    let (harness, app) = Harness::without_runtime().await;

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{}/prompt", harness.session),
        Some(json!({ "text": "stored only" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{}/interrupt", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({ "interrupted": false }));

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/session/{}/wait", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.unwrap()["_tag"], "ServiceUnavailableError");
}
