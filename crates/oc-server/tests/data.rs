//! Scenario tests for the data routes through the real router.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use oc_config::{ConfigService, LoadOptions};
use oc_server::{AppState, router};
use oc_store::Store;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    _db: tempfile::TempDir,
    _config: tempfile::TempDir,
    state: AppState,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        config_dir.path().join("opencode.json"),
        r#"{"model":"test/model","agent":{"build":{"model":"test/model"}}}"#,
    )
    .unwrap();
    let skill = config_dir.path().join(".opencode/skill/demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: Demo\ndescription: demo skill\n---\nDo the demo.\n",
    )
    .unwrap();
    let command = config_dir.path().join(".opencode/command");
    std::fs::create_dir_all(&command).unwrap();
    std::fs::write(
        command.join("deploy.md"),
        "---\ndescription: deploy things\n---\nDeploy $ARGUMENTS now.\n",
    )
    .unwrap();
    let home = config_dir.path().to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert("HOME".to_owned(), home.clone());
    env.insert("USERPROFILE".to_owned(), home.clone());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        config_dir.path().join("xdg").to_string_lossy().into_owned(),
    );
    // Shadow ambient inline config; whitespace is ignored by the loader.
    env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
    let config = ConfigService::load(LoadOptions {
        directory: config_dir.path().to_path_buf(),
        env,
    })
    .unwrap();

    let state = AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        config_dir.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
    .with_config(Arc::new(config));

    Harness {
        _db: db,
        _config: config_dir,
        state,
    }
}

async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

async fn create_session(app: &axum::Router, body: Value) -> Value {
    let (status, value) = call(app, Method::POST, "/api/session", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "create failed: {value:?}");
    value.unwrap()["data"].clone()
}

fn session_id(session: &Value) -> String {
    session["id"].as_str().unwrap().to_owned()
}

struct SseReader {
    body: axum::body::Body,
    buffer: String,
}

impl SseReader {
    fn new(response: axum::response::Response) -> Self {
        Self {
            body: response.into_body(),
            buffer: String::new(),
        }
    }

    async fn next_event(&mut self) -> Option<Value> {
        loop {
            if let Some(index) = self.buffer.find("\n\n") {
                let chunk = self.buffer[..index].to_owned();
                self.buffer.drain(..index + 2);
                for line in chunk.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        return Some(serde_json::from_str(data).unwrap());
                    }
                }
                continue;
            }
            let frame = tokio::time::timeout(Duration::from_secs(3), self.body.frame())
                .await
                .ok()??
                .ok()?;
            let data = frame.into_data().ok()?;
            self.buffer.push_str(&String::from_utf8_lossy(&data));
        }
    }
}

async fn sse(app: &axum::Router, uri: &str) -> axum::response::Response {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    response
}

#[tokio::test]
async fn create_list_get_patch_delete() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let created = create_session(
        &app,
        json!({"title": "first", "metadata": {"a": 1}, "agent": "build"}),
    )
    .await;
    let id = session_id(&created);
    assert!(id.starts_with("ses_"));
    assert_eq!(created["title"], "first");
    assert_eq!(created["agent"], "build");
    assert!(created["projectID"].as_str().unwrap().starts_with("prj_"));
    assert!(created["location"]["directory"].is_string());

    let (status, body) = call(&app, Method::GET, &format!("/api/session/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["id"], id);

    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{id}"),
        Some(json!({"title": "renamed", "archived": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (_, body) = call(&app, Method::GET, &format!("/api/session/{id}"), None).await;
    let session = body.unwrap()["data"].clone();
    assert_eq!(session["title"], "renamed");
    assert!(session["time"]["archived"].is_i64());

    let (status, body) = call(&app, Method::GET, "/api/session", None).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_eq!(body["data"][0]["id"], id);
    assert!(body["cursor"].is_object());

    let (status, _) = call(&app, Method::DELETE, &format!("/api/session/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = call(&app, Method::GET, &format!("/api/session/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "SessionNotFoundError");
}

#[tokio::test]
async fn parent_filter_selects_roots_and_children() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let root = create_session(&app, json!({"title": "root"})).await;
    let root_id = session_id(&root);
    let child = create_session(&app, json!({"title": "child", "parentID": root_id})).await;
    assert_eq!(child["parentID"], root_id);

    let (_, body) = call(&app, Method::GET, "/api/session?parentID=null", None).await;
    let body = body.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], root_id);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session?parentID={root_id}"),
        None,
    )
    .await;
    let body = body.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["id"], child["id"]);
}

#[tokio::test]
async fn message_pagination_and_lookup() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let session = create_session(&app, json!({})).await;
    let sid = session_id(&session);

    for (index, id) in ["msg_a", "msg_b", "msg_c"].iter().enumerate() {
        let (status, _) = call(
            &app,
            Method::POST,
            &format!("/api/session/{sid}/prompt"),
            Some(json!({"id": id, "text": format!("prompt {index}")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?limit=2&order=asc"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    let first_page = body["data"].as_array().unwrap();
    assert_eq!(first_page.len(), 2);
    assert_eq!(first_page[0]["id"], "msg_a");
    assert_eq!(first_page[0]["type"], "user");
    let next = body["cursor"]["next"].as_str().unwrap().to_owned();

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?limit=2&order=asc&cursor={next}"),
        None,
    )
    .await;
    let body = body.unwrap();
    let second_page = body["data"].as_array().unwrap();
    assert_eq!(second_page.len(), 1);
    assert_eq!(second_page[0]["id"], "msg_c");

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message/msg_b"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["text"], "prompt 1");

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message/msg_missing"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "MessageNotFoundError");
}

#[tokio::test]
async fn queued_prompt_stays_inbox_only_until_delivery() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({
            "id": "msg_prompt1",
            "text": "hello",
            "metadata": {"draft": true},
            "delivery": "queue",
            "files": [{"uri": "file:///tmp/a.txt"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let item = body.unwrap()["data"].clone();
    assert_eq!(item["id"], "msg_prompt1");
    assert_eq!(item["sessionID"], sid);
    assert_eq!(item["type"], "user");
    assert_eq!(item["delivery"], "queue");
    assert_eq!(item["payload"]["text"], "hello");
    assert_eq!(item["payload"]["metadata"]["draft"], true);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    assert_eq!(inbox["data"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["data"][0]["id"], "msg_prompt1");

    // A queued prompt is not part of the transcript until the runtime promotes
    // its inbox row; materialising it here would leak it into a running turn.
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    assert!(
        messages["data"].as_array().unwrap().is_empty(),
        "queued prompt materialised early: {messages}"
    );

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=0")).await;
    let mut reader = SseReader::new(stream);
    let mut events = Vec::new();
    while let Some(event) = reader.next_event().await {
        events.push(event);
    }
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec!["session.created", "session.inbox.enqueued", "log.synced"]
    );
    assert_eq!(events[1]["durable"]["aggregateID"], sid);
    assert_eq!(events[1]["data"]["inboxID"], "msg_prompt1");
    assert_eq!(events[1]["data"]["item"]["delivery"], "queue");
    assert!(
        events[1]["durable"]["seq"].as_i64().unwrap()
            > events[0]["durable"]["seq"].as_i64().unwrap()
    );
}

#[tokio::test]
async fn steer_prompt_materialises_message_immediately() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"id": "msg_steer1", "text": "steer me", "delivery": "steer"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    let messages = body.unwrap();
    let messages = messages["data"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["id"], "msg_steer1");
    assert_eq!(messages[0]["type"], "user");
    assert_eq!(messages[0]["text"], "steer me");

    // The inbox row is only the doorbell once the message exists.
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    assert_eq!(inbox["data"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["data"][0]["delivery"], "steer");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"id": "msg_steer1", "text": "again"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["_tag"], "ConflictError");
}

#[tokio::test]
async fn duplicate_queued_prompt_id_conflicts_on_the_inbox_row() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let request = json!({"id": "msg_queued1", "text": "queued", "delivery": "queue"});
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn queued_command_is_not_materialised() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "deploy", "text": "prod", "delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    let inbox = body.unwrap();
    let items = inbox["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "user");
    assert_eq!(items[0]["delivery"], "queue");
    assert!(
        items[0]["payload"]["text"]
            .as_str()
            .unwrap()
            .contains("Deploy prod now.")
    );

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/message?order=asc"),
        None,
    )
    .await;
    assert!(
        body.unwrap()["data"].as_array().unwrap().is_empty(),
        "queued command materialised early"
    );
}

#[tokio::test]
async fn duplicate_prompt_id_conflicts() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let request = json!({"id": "msg_dupe", "text": "first"});
    let (status, _) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"text": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "BadRequestError");
}

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

#[tokio::test]
async fn log_replays_in_order_and_follows_live() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({"title": "one"})).await);

    let (status, _) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}"),
        Some(json!({"title": "two"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=0&follow=true")).await;
    let mut reader = SseReader::new(stream);

    let first = reader.next_event().await.unwrap();
    let second = reader.next_event().await.unwrap();
    let marker = reader.next_event().await.unwrap();
    assert_eq!(first["type"], "session.created");
    assert_eq!(second["type"], "session.renamed");
    assert_eq!(second["data"]["title"], "two");
    let first_seq = first["durable"]["seq"].as_i64().unwrap();
    let second_seq = second["durable"]["seq"].as_i64().unwrap();
    assert!(second_seq > first_seq);
    assert_eq!(marker["type"], "log.synced");
    assert_eq!(marker["aggregateID"], sid);
    assert_eq!(marker["seq"], second_seq);

    let (status, _) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}"),
        Some(json!({"title": "three"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let live = reader.next_event().await.unwrap();
    assert_eq!(live["type"], "session.renamed");
    assert_eq!(live["data"]["title"], "three");
    assert_eq!(live["durable"]["seq"].as_i64().unwrap(), second_seq + 1);
}

#[tokio::test]
async fn log_after_skips_replayed_events() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({"title": "one"})).await);
    call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}"),
        Some(json!({"title": "two"})),
    )
    .await;

    let stream = sse(&app, &format!("/api/session/{sid}/log?after=1")).await;
    let mut reader = SseReader::new(stream);
    let renamed = reader.next_event().await.unwrap();
    let marker = reader.next_event().await.unwrap();
    assert_eq!(renamed["type"], "session.renamed");
    assert_eq!(renamed["durable"]["seq"], 2);
    assert_eq!(marker["seq"], 2);
    assert!(reader.next_event().await.is_none());
}

#[tokio::test]
async fn inbox_delivery_can_change_and_cancel() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (_, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/prompt"),
        Some(json!({"id": "msg_inbox", "text": "hold"})),
    )
    .await;
    assert_eq!(body.unwrap()["data"]["delivery"], "steer");

    let (status, _) = call(
        &app,
        Method::PATCH,
        &format!("/api/session/{sid}/inbox/msg_inbox"),
        Some(json!({"delivery": "queue"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    assert_eq!(body.unwrap()["data"][0]["delivery"], "queue");

    let (status, _) = call(
        &app,
        Method::DELETE,
        &format!("/api/session/{sid}/inbox/msg_inbox"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = call(
        &app,
        Method::GET,
        &format!("/api/session/{sid}/inbox"),
        None,
    )
    .await;
    assert!(body.unwrap()["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn synthetic_compact_and_unimplemented_runtimes() {
    let harness = harness().await;
    let app = router(harness.state.clone());
    let sid = session_id(&create_session(&app, json!({})).await);

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/synthetic"),
        Some(json!({"id": "msg_syn", "text": "note", "description": "why"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_eq!(body["data"]["type"], "synthetic");
    assert_eq!(body["data"]["payload"]["text"], "note");

    // A retried synthetic id that already became a durable message is a
    // conflict, not a silent no-op: the actor's doorbell branch would consume
    // the second inbox row without executing it.
    let store = harness.state.store().expect("store");
    store
        .append_message(oc_store::NewMessage {
            id: "msg_syn".to_owned(),
            session_id: sid.clone(),
            kind: "synthetic".to_owned(),
            data: json!({"text": "note"}),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("durable message");
    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/synthetic"),
        Some(json!({"id": "msg_syn", "text": "note"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/compact"),
        Some(json!({"id": "msg_cmp"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"]["type"], "compaction");

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/shell"),
        Some(json!({"command": "ls"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none());

    let (status, body) = call(
        &app,
        Method::POST,
        &format!("/api/session/{sid}/command"),
        Some(json!({"name": "missing", "text": "y"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "CommandNotFoundError");
}

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
