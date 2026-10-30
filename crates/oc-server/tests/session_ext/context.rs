use axum::http::{Method, StatusCode};
use oc_server::router;
use oc_types::now_millis;
use serde_json::json;

use crate::support::{call, create_session, harness, prompt, session_id, take_events};

#[tokio::test]
async fn context_environment_and_view() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "context"})).await;
    let sid = session_id(&session);
    prompt(&app, &sid, "hello").await;

    let (status, context) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/context"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(context.unwrap()["data"].as_array().unwrap().len(), 1);

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/session/{sid}/environment"),
        Some(json!({"variables": {"FOO": "bar"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(
        session_body.unwrap()["data"]["metadata"]["environment"],
        json!({"FOO": "bar"})
    );

    let idle = now_millis();
    let mut events = harness.state.bus().subscribe();
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/view"),
        Some(json!({"idle": idle})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let published = take_events(&mut events, "session.viewed");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].data, json!({ "sessionID": sid, "idle": idle }));
    let durable = published[0].durable.as_ref().expect("viewed is durable");
    assert_eq!(durable.version, 1);
    assert_eq!(durable.aggregate_id, sid);
    assert!(durable.seq >= 0);
    assert_eq!(
        published[0].location.as_ref().unwrap().directory,
        session["location"]["directory"].as_str().unwrap()
    );

    let (_, session_body) = call(&app, Method::GET, &format!("/api/session/{sid}"), None).await;
    assert_eq!(session_body.unwrap()["data"]["time"]["viewed"], idle);

    let (status, _) = call(
        &app,
        Method::PUT,
        &format!("/api/session/{sid}/environment"),
        Some(json!({"variables": {"FOO": 1}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
