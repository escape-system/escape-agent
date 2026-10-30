use axum::http::{Method, StatusCode};
use oc_server::router;
use oc_store::NewMessage;
use oc_types::now_millis;
use serde_json::json;

use crate::support::{call, create_session, harness, prompt, session_id};

#[tokio::test]
async fn stats_aggregate_messages() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({"title": "stats"})).await;
    let sid = session_id(&session);
    prompt(&app, &sid, "question").await;
    let now = now_millis();
    let store = harness.state.store().unwrap();
    store
        .append_message(NewMessage {
            id: "msg_stats_assistant".to_owned(),
            session_id: sid.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "model": {"providerID": "test", "id": "model"},
                "tokens": {
                    "input": 10,
                    "output": 5,
                    "reasoning": 1,
                    "cache": {"read": 2, "write": 3}
                },
                "cost": 0.5,
                "time": {"created": now},
                "content": [{
                    "type": "tool",
                    "id": "call_1",
                    "name": "read",
                    "state": {"status": "completed"},
                    "time": {"created": now, "ran": now, "completed": now + 4}
                }],
            }),
            time_created: now,
        })
        .await
        .unwrap();

    let until = now_millis() + 1_000_000;
    let (code, stats) = call(
        &app,
        Method::GET,
        &format!("/api/experimental/session/stats?from=0&to={until}&tools=detail"),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "stats: {stats:?}");
    let stats = stats.unwrap()["data"].clone();
    assert_eq!(stats["prompts"], 1);
    assert_eq!(stats["steps"], 1);
    assert_eq!(stats["sessions"], 1);
    assert_eq!(stats["tokens"]["input"], 10);
    assert_eq!(stats["tokens"]["output"], 5);
    assert_eq!(stats["tokens"]["cache"]["read"], 2);
    assert_eq!(stats["cost"], 0.5);
    assert_eq!(stats["tools"]["totals"]["calls"], 1);
    assert_eq!(stats["tools"]["totals"]["succeeded"], 1);
    assert_eq!(stats["tools"]["usage"][0]["name"], "read");
    assert_eq!(stats["tools"]["usage"][0]["durationP50"], 4.0);
    assert_eq!(stats["models"][0]["model"]["providerID"], "test");
    assert_eq!(stats["activeDays"], 1);
    assert_eq!(stats["streak"], 1);
    assert_eq!(stats["range"]["from"], 0);

    let (code, _) = call(
        &app,
        Method::GET,
        "/api/experimental/session/stats?from=10&to=5",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}
