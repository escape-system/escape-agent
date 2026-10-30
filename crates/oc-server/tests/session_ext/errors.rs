use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{call, harness};

#[tokio::test]
async fn missing_sessions_are_404() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    for uri in [
        "/api/session/ses_absent/context",
        "/api/session/ses_absent/diff",
        "/api/session/ses_absent/form",
        "/api/experimental/session/ses_absent/instructions/entries",
    ] {
        let (status, body) = call(&app, Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body:?}");
        assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");
    }
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/session/ses_absent/view",
        Some(json!({"idle": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
