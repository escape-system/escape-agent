use axum::http::{Method, StatusCode};
use oc_server::router;
use serde_json::json;

use crate::support::{call, create_session, harness, session_id};

#[tokio::test]
async fn skill_activation_enqueues_skill_content() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "skill"})).await;
    let sid = session_id(&session);

    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/session/{sid}/skill"),
        Some(json!({"id": "demo", "resume": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, inbox) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = inbox.unwrap()["data"][0].clone();
    assert_eq!(item["type"], "synthetic");
    let text = item["payload"]["text"].as_str().unwrap();
    assert!(text.contains("<skill_content name=\"Demo\">"));
    assert!(text.contains("Do the demo."));

    let (status, missing) = call(
        &app,
        Method::POST,
        &format!("/api/experimental/session/{sid}/skill"),
        Some(json!({"id": "absent"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing.unwrap()["_tag"], "SkillNotFoundError");
}

#[tokio::test]
async fn instruction_entries_round_trip_and_limit() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "instructions"})).await;
    let sid = session_id(&session);

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/experimental/session/{sid}/instructions/entries/notes"),
        Some(json!({"value": {"a": 1}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, entries) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/instructions/entries"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = entries.unwrap();
    assert_eq!(entries["data"][0]["key"], "notes");
    assert_eq!(entries["data"][0]["value"], json!({"a": 1}));

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/experimental/session/{sid}/instructions/entries/Bad%20Key"),
        Some(json!({"value": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, too_large) = call(
        &app,
        Method::PUT,
        &format!("/api/experimental/session/{sid}/instructions/entries/huge"),
        Some(json!({"value": "x".repeat(1024 * 1024 + 16)})),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let too_large = too_large.unwrap();
    assert_eq!(too_large["_tag"], "InstructionEntryValueTooLargeError");
    assert_eq!(too_large["maxBytes"], 1_048_576);
    assert!(too_large["actualBytes"].as_u64().unwrap() > 1_048_576);

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/experimental/session/{sid}/instructions/entries/notes"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, entries) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/{sid}/instructions/entries"),
        None,
    )
    .await;
    assert_eq!(entries.unwrap()["data"], json!([]));
}
