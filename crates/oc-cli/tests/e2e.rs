//! End-to-end tests through the real `opencode` binary.
//!
//! Every test owns an isolated temp tree and a loopback fake provider. The
//! tests run serially because each one binds ports and spawns a process.

use std::time::Duration;

use oc_testkit::{
    FakeProvider, Script, ServerHarness, SseReader, TestConfig, assistant_text, create_session,
    events_after, json_request, prompt, wait_for_execution,
};
use reqwest::Method;
use serde_json::{Value, json};

const TEXT: &str = "Hello from the fake provider";

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn config(fake: &FakeProvider) -> TestConfig {
    TestConfig::new().with_provider(fake.base_url())
}

fn tool_parts(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|message| message["type"] == "assistant")
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "tool")
        .cloned()
        .collect()
}

fn has_event(events: &[Value], kind: &str) -> bool {
    events.iter().any(|event| event["type"] == kind)
}

#[tokio::test]
async fn text_turn_completes_and_records_success() {
    let _guard = LOCK.lock().await;
    let fake = FakeProvider::start(Script::Text(TEXT.to_owned()))
        .await
        .unwrap();
    let mut harness = ServerHarness::start(env!("CARGO_BIN_EXE_opencode"), config(&fake))
        .await
        .unwrap();
    let client = harness.client().clone();
    let base = harness.base_url().to_owned();

    let session = create_session(&client, &base).await.unwrap();
    prompt(&client, &base, &session, "say hello").await.unwrap();
    let messages = wait_for_execution(&client, &base, &session).await.unwrap();

    assert_eq!(assistant_text(&messages).as_deref(), Some(TEXT));
    assert_eq!(fake.requests().len(), 1, "one model call for a text turn");

    let (_, active) = json_request(
        &client,
        Method::GET,
        &format!("{base}/api/session/active"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        active["data"],
        json!({}),
        "no active execution after settle"
    );

    let events = events_after(&client, &base, &session).await.unwrap();
    assert!(
        has_event(&events, "session.execution.succeeded"),
        "{events:?}"
    );
    harness.stop().await;
}

#[tokio::test]
async fn tool_turn_reads_a_real_file() {
    let _guard = LOCK.lock().await;
    let fake = FakeProvider::start(Script::ToolThenText {
        tool: "read".to_owned(),
        arguments: json!({"path": "hello.txt"}),
        text: "the file says hello".to_owned(),
    })
    .await
    .unwrap();
    let mut harness = ServerHarness::start(env!("CARGO_BIN_EXE_opencode"), config(&fake))
        .await
        .unwrap();
    let client = harness.client().clone();
    let base = harness.base_url().to_owned();
    std::fs::write(harness.directory().join("hello.txt"), "hello world\n").unwrap();

    let session = create_session(&client, &base).await.unwrap();
    prompt(&client, &base, &session, "read hello.txt")
        .await
        .unwrap();
    let messages = wait_for_execution(&client, &base, &session).await.unwrap();

    assert_eq!(
        assistant_text(&messages).as_deref(),
        Some("the file says hello")
    );
    let tools = tool_parts(&messages);
    assert_eq!(tools.len(), 1, "{messages:?}");
    assert_eq!(tools[0]["state"]["status"], "completed");
    assert_eq!(tools[0]["state"]["input"]["path"], "hello.txt");

    let requests = fake.requests();
    assert_eq!(requests.len(), 2, "tool call then final answer");
    assert!(
        requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool"),
        "second request carries the tool result"
    );

    let events = events_after(&client, &base, &session).await.unwrap();
    assert!(has_event(&events, "session.tool.called"), "{events:?}");
    assert!(has_event(&events, "session.tool.success"), "{events:?}");
    assert!(
        has_event(&events, "session.execution.succeeded"),
        "{events:?}"
    );
    harness.stop().await;
}

#[tokio::test]
async fn permission_ask_is_answered_once() {
    let _guard = LOCK.lock().await;
    let fake = FakeProvider::start(Script::ToolThenText {
        tool: "shell".to_owned(),
        arguments: json!({"command": "echo ok"}),
        text: "done".to_owned(),
    })
    .await
    .unwrap();
    let mut harness = ServerHarness::start(
        env!("CARGO_BIN_EXE_opencode"),
        config(&fake).with_permission("shell", "*", "ask"),
    )
    .await
    .unwrap();
    let client = harness.client().clone();
    let base = harness.base_url().to_owned();

    let session = create_session(&client, &base).await.unwrap();
    let mut subscription = SseReader::open(&client, &format!("{base}/api/event"))
        .await
        .unwrap();
    prompt(&client, &base, &session, "run echo ok")
        .await
        .unwrap();

    let asked = subscription
        .until(Duration::from_secs(10), |event| {
            event["type"] == "permission.asked"
        })
        .await
        .unwrap();
    assert_eq!(asked["data"]["sessionID"], session.as_str());
    assert_eq!(asked["data"]["action"], "shell");
    let request_id = asked["data"]["id"].as_str().unwrap();

    let (status, body) = json_request(
        &client,
        Method::POST,
        &format!("{base}/api/session/{session}/permission/{request_id}/reply"),
        Some(json!({"decision": "once"})),
    )
    .await
    .unwrap();
    assert!(status.is_success(), "{status}: {body}");

    let messages = wait_for_execution(&client, &base, &session).await.unwrap();
    assert_eq!(assistant_text(&messages).as_deref(), Some("done"));
    let tools = tool_parts(&messages);
    assert_eq!(tools.len(), 1, "{messages:?}");
    assert_eq!(tools[0]["state"]["status"], "completed");

    let events = events_after(&client, &base, &session).await.unwrap();
    assert!(has_event(&events, "session.tool.success"), "{events:?}");
    harness.stop().await;
}

#[tokio::test]
async fn restart_keeps_durable_messages() {
    let _guard = LOCK.lock().await;
    let fake = FakeProvider::start(Script::Text("persisted answer".to_owned()))
        .await
        .unwrap();
    let mut harness = ServerHarness::start(env!("CARGO_BIN_EXE_opencode"), config(&fake))
        .await
        .unwrap();
    let client = harness.client().clone();
    let base = harness.base_url().to_owned();

    let session = create_session(&client, &base).await.unwrap();
    prompt(&client, &base, &session, "remember this")
        .await
        .unwrap();
    let messages = wait_for_execution(&client, &base, &session).await.unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["type"] == "assistant")
        .expect("assistant message");
    let message_id = assistant["id"].as_str().unwrap().to_owned();

    harness.stop().await;
    harness.restart().await.unwrap();
    let base = harness.base_url().to_owned();
    let (status, body) = json_request(
        &client,
        Method::GET,
        &format!("{base}/api/session/{session}/message/{message_id}"),
        None,
    )
    .await
    .unwrap();
    assert!(status.is_success(), "{status}: {body}");
    assert_eq!(body["data"]["id"], message_id.as_str());
    assert_eq!(body["data"]["type"], "assistant");
    harness.stop().await;
}

#[tokio::test]
async fn serves_web_dist_fixture_with_immutable_assets() {
    let _guard = LOCK.lock().await;
    let web = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(web.path().join("_assets")).unwrap();
    std::fs::write(
        web.path().join("index.html"),
        "<!doctype html><title>fixture</title>\n",
    )
    .unwrap();
    std::fs::write(
        web.path().join("_assets/app-abc123.js"),
        "console.log('fixture');\n",
    )
    .unwrap();

    let config =
        TestConfig::new().with_env("OC_WEB_DIST", web.path().to_string_lossy().into_owned());
    let mut harness = ServerHarness::start(env!("CARGO_BIN_EXE_opencode"), config)
        .await
        .unwrap();
    let client = harness.client().clone();
    let base = harness.base_url().to_owned();

    let index = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index.status(), 200);
    assert!(index.text().await.unwrap().contains("fixture"));

    let asset = client
        .get(format!("{base}/_assets/app-abc123.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), 200);
    let cache = asset
        .headers()
        .get("cache-control")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(cache.contains("immutable"), "{cache}");
    assert_eq!(asset.text().await.unwrap(), "console.log('fixture');\n");
    harness.stop().await;
}
