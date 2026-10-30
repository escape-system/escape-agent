use std::sync::Arc;

use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::support::{
    Gate, Harness, Script, assistants, call, execution_starts, expectation, requests_mention,
    text_script, tool_events, user_texts,
};

#[tokio::test]
async fn queued_prompt_is_invisible_to_the_running_turn_and_runs_once() {
    let gate = Arc::new(Gate::default());
    let queued = "queued prompt text";
    let harness = Harness::new(
        vec![
            Script::Gated {
                events: tool_events(
                    "call_read",
                    "read",
                    json!({"path": "hello.txt", "expectation": expectation()}),
                ),
                gate: Arc::clone(&gate),
            },
            text_script("first turn done"),
            text_script("queued turn done"),
        ],
        None,
    )
    .await;

    let (status, _) = harness.prompt("start").await;
    assert_eq!(status, StatusCode::OK);
    gate.wait_entered().await;
    assert_eq!(harness.model.calls(), 1);

    // Admit while the first step of the first turn is still waiting on the
    // provider.
    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/prompt", harness.session),
        Some(json!({"id": "msg_queued", "text": queued, "delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    let messages = harness.messages().await;
    assert_eq!(user_texts(&messages), vec!["start".to_owned()]);
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        !requests_mention(&requests[0], queued),
        "queued prompt leaked into the running turn's request"
    );

    let (_, body) = call(
        &harness.app,
        Method::GET,
        &format!("/api/session/{}/inbox", harness.session),
        None,
    )
    .await;
    let inbox = body.unwrap();
    assert_eq!(inbox["data"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["data"][0]["id"], "msg_queued");
    assert_eq!(inbox["data"][0]["delivery"], "queue");

    gate.release();
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    // The second step of the first turn still must not see the queued text.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 3, "requests: {}", requests.len());
    assert!(
        !requests_mention(&requests[1], queued),
        "queued prompt leaked into the first turn's next step"
    );
    assert!(
        requests_mention(&requests[2], queued),
        "queued prompt was never delivered to a new turn"
    );

    let messages = harness.messages().await;
    assert_eq!(
        user_texts(&messages),
        vec!["start".to_owned(), queued.to_owned()],
        "queued prompt must appear exactly once after the turn settles"
    );
    assert_eq!(harness.model.calls(), 3);
    assert_eq!(execution_starts(&harness.app, &harness.session).await, 2);

    let (_, body) = call(
        &harness.app,
        Method::GET,
        &format!("/api/session/{}/inbox", harness.session),
        None,
    )
    .await;
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn steer_prompt_joins_the_running_turn_without_duplication() {
    let gate = Arc::new(Gate::default());
    let steer = "steer prompt text";
    let harness = Harness::new(
        vec![
            Script::Gated {
                events: tool_events(
                    "call_read",
                    "read",
                    json!({"path": "hello.txt", "expectation": expectation()}),
                ),
                gate: Arc::clone(&gate),
            },
            text_script("steer handled"),
        ],
        None,
    )
    .await;

    let (status, _) = harness.prompt("start").await;
    assert_eq!(status, StatusCode::OK);
    gate.wait_entered().await;
    assert_eq!(harness.model.calls(), 1);

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/prompt", harness.session),
        Some(json!({"id": "msg_steer", "text": steer, "delivery": "steer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    // Steer is materialised immediately, so the host transcript already has it;
    // the running request was built before, so it does not.
    let messages = harness.messages().await;
    assert_eq!(
        user_texts(&messages),
        vec!["start".to_owned(), steer.to_owned()]
    );
    assert!(!requests_mention(&harness.model.requests()[0], steer));

    gate.release();
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "requests: {}", requests.len());
    assert!(
        requests_mention(&requests[1], steer),
        "steer prompt did not reach the running turn's next step"
    );

    let messages = harness.messages().await;
    assert_eq!(
        user_texts(&messages),
        vec!["start".to_owned(), steer.to_owned()],
        "steer prompt duplicated after settle"
    );
    assert_eq!(harness.model.calls(), 2);
    assert_eq!(execution_starts(&harness.app, &harness.session).await, 1);

    let (_, body) = call(
        &harness.app,
        Method::GET,
        &format!("/api/session/{}/inbox", harness.session),
        None,
    )
    .await;
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn queued_prompt_on_an_idle_session_is_delivered_once() {
    let harness = Harness::new(vec![text_script("idle answer")], None).await;

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/prompt", harness.session),
        Some(json!({"id": "msg_idle_queue", "text": "idle queued", "delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    assert_eq!(user_texts(&messages), vec!["idle queued".to_owned()]);
    assert_eq!(assistants(&messages).len(), 1);
    assert_eq!(harness.model.calls(), 1);
    assert_eq!(execution_starts(&harness.app, &harness.session).await, 1);
    assert!(requests_mention(
        &harness.model.requests()[0],
        "idle queued"
    ));

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn queued_command_waits_for_the_running_turn() {
    let gate = Arc::new(Gate::default());
    let harness = Harness::new(
        vec![
            Script::Gated {
                events: tool_events(
                    "call_read",
                    "read",
                    json!({"path": "hello.txt", "expectation": expectation()}),
                ),
                gate: Arc::clone(&gate),
            },
            text_script("first turn done"),
            text_script("queued command done"),
        ],
        None,
    )
    .await;

    let (status, _) = harness.prompt("start").await;
    assert_eq!(status, StatusCode::OK);
    gate.wait_entered().await;
    assert_eq!(harness.model.calls(), 1);

    let (status, body) = call(
        &harness.app,
        Method::POST,
        &format!("/api/session/{}/command", harness.session),
        Some(json!({"name": "deploy", "text": "prod", "delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    assert_eq!(user_texts(&messages), vec!["start".to_owned()]);
    assert!(!requests_mention(
        &harness.model.requests()[0],
        "Deploy prod now."
    ));

    gate.release();
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let texts = user_texts(&messages);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert_eq!(texts[0], "start");
    assert!(texts[1].contains("Deploy prod now."));
    assert_eq!(harness.model.calls(), 3);
    assert_eq!(execution_starts(&harness.app, &harness.session).await, 2);

    harness.runtime.expect("runtime").shutdown().await;
}
