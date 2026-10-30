//! End-to-end tests for the server ↔ runtime boundary.
//!
//! Every test drives the real router with a scripted [`ModelClient`], a temp
//! SQLite store and a temp config tree. No HTTP request leaves the process.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::body::to_bytes;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Method, Request, StatusCode};
use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{
    Catalog, ContentPart, LlmClient, LlmError, LlmEvent, LlmMessage, LlmRequest, LlmStream,
    SharedLlmClient, Usage,
};
use oc_runtime::{ModelClient, Runtime, RuntimeConfig};
use oc_server::{AppState, BusSink, PermissionBroker, router};
use oc_store::{NewSession, Store};
use oc_tools::ToolRegistry;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const CATALOG: &str = r#"{
  "test": {
    "id": "test",
    "name": "Test Provider",
    "env": ["TEST_API_KEY"],
    "models": {
      "alpha": {
        "id": "alpha",
        "type": "chat",
        "name": "Alpha",
        "release_date": "2025-01-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "temperature": true},
        "limit": {"context": 100000, "output": 10000},
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
        "api": {
          "chat-completions": {
            "base_url": "http://unused.invalid",
            "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
          }
        }
      }
    }
  }
}"#;

#[derive(Clone)]
enum Script {
    Events(Vec<LlmEvent>),
    HangAfter(Vec<LlmEvent>),
    /// Signals `entered` when the provider call starts and holds the response
    /// until `release`, so a test can admit input mid-step.
    Gated {
        events: Vec<LlmEvent>,
        gate: Arc<Gate>,
    },
}

#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Gate {
    /// Waits until the provider call holding this gate starts.
    async fn wait_entered(&self) {
        self.entered.notified().await;
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

struct ScriptedModel {
    scripts: Mutex<VecDeque<Script>>,
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<LlmMessage>>>,
}

impl ScriptedModel {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Vec<LlmMessage>> {
        self.requests.lock().expect("requests").clone()
    }
}

impl ModelClient for ScriptedModel {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .expect("requests")
            .push(request.messages);
        let script = self
            .scripts
            .lock()
            .expect("scripts")
            .pop_front()
            .unwrap_or_else(|| Script::Events(Vec::new()));
        Box::pin(async move {
            match script {
                Script::Events(events) => Ok(stream::iter(events).boxed()),
                Script::HangAfter(events) => Ok(stream::iter(events)
                    .chain(stream::pending::<LlmEvent>())
                    .boxed()),
                Script::Gated { events, gate } => {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                    Ok(stream::iter(events).boxed())
                }
            }
        })
    }
}

struct Harness {
    _root: TempDir,
    work: PathBuf,
    session: String,
    app: axum::Router,
    runtime: Option<Arc<Runtime>>,
    model: Arc<ScriptedModel>,
    store: Arc<Store>,
    config: Arc<ConfigService>,
    catalog: Arc<Catalog>,
}

impl Harness {
    async fn new(scripts: Vec<Script>, session_permissions: Option<Value>) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        std::fs::write(work.join("hello.txt"), "hello world\n").expect("file");
        let command = work.join(".opencode/command");
        std::fs::create_dir_all(&command).expect("command dir");
        std::fs::write(
            command.join("deploy.md"),
            "---\ndescription: deploy things\n---\nDeploy $ARGUMENTS now.\n",
        )
        .expect("command file");

        let config = ConfigService::load(
            LoadOptions::new(&work)
                .with_env("HOME", path(root.path()))
                .with_env("USERPROFILE", path(root.path()))
                .with_env("XDG_CONFIG_HOME", path(&root.path().join("config")))
                .with_env("ProgramData", path(&root.path().join("programdata")))
                .with_env("OPENCODE_CONFIG", path(&root.path().join("missing.json")))
                .with_env("OPENCODE_CONFIG_CONTENT", "{}"),
        )
        .expect("config loads");

        let store = Arc::new(
            Store::open(root.path().join("db.sqlite"))
                .await
                .expect("store opens"),
        );
        let session = "ses_server_runtime".to_owned();
        store
            .create_session(NewSession {
                id: session.clone(),
                parent_id: None,
                project_id: "prj_server_runtime".to_owned(),
                directory: path(&work),
                title: Some("server runtime".to_owned()),
                agent: Some("build".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: session_permissions,
                time_created: oc_types::now_millis(),
            })
            .await
            .expect("session created");

        let config = Arc::new(config);
        let catalog = Arc::new(Catalog::from_snapshot(CATALOG).expect("catalog"));
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
            work.clone(),
        )
        .with_store(Arc::clone(&store))
        .with_config(Arc::clone(&config))
        .with_catalog(Arc::clone(&catalog));

        let broker = Arc::new(PermissionBroker::from_state(&state));
        let sink = Arc::new(BusSink::from_state(&state));
        let model = ScriptedModel::new(scripts);
        let runtime = Arc::new(Runtime::new_with_model_client(
            RuntimeConfig {
                store: Arc::clone(&store),
                config: Arc::clone(&config),
                catalog: Arc::clone(&catalog),
                client: Arc::new(SharedLlmClient::new(LlmClient::new().expect("client"))),
                tools: Arc::new(ToolRegistry::with_defaults()),
                sink,
                permissions: broker.clone(),
            },
            Arc::clone(&model) as Arc<dyn ModelClient>,
        ));
        runtime.start().await.expect("runtime starts");

        let app = router(
            state
                .with_runtime(Arc::clone(&runtime))
                .with_permissions(broker),
        );
        Self {
            _root: root,
            work,
            session,
            app,
            runtime: Some(runtime),
            model,
            store,
            config,
            catalog,
        }
    }

    async fn without_runtime() -> (Self, axum::Router) {
        let mut harness = Self::new(Vec::new(), None).await;
        let runtime = harness.runtime.take().expect("runtime");
        runtime.shutdown().await;
        let app = router(
            AppState::new(
                ServerInfo {
                    version: "0.0.0-test".to_owned(),
                    pid: 4242,
                    hostname: None,
                    urls: vec![],
                    paths: ServerPaths {
                        tmp: std::env::temp_dir().to_string_lossy().into_owned(),
                    },
                    capabilities: None,
                },
                None,
                harness.work.clone(),
            )
            .with_store(Arc::clone(&harness.store))
            .with_config(Arc::clone(&harness.config))
            .with_catalog(Arc::clone(&harness.catalog)),
        );
        (harness, app)
    }

    async fn prompt(&self, text: &str) -> (StatusCode, Option<Value>) {
        call(
            &self.app,
            Method::POST,
            &format!("/api/session/{}/prompt", self.session),
            Some(json!({ "text": text })),
        )
        .await
    }

    async fn wait(&self) -> (StatusCode, Option<Value>) {
        call(
            &self.app,
            Method::POST,
            &format!("/api/experimental/session/{}/wait?timeout=10", self.session),
            None,
        )
        .await
    }

    async fn messages(&self) -> Vec<Value> {
        let (_, body) = call(
            &self.app,
            Method::GET,
            &format!("/api/session/{}/message?order=asc", self.session),
            None,
        )
        .await;
        body.expect("messages")["data"]
            .as_array()
            .expect("array")
            .clone()
    }

    async fn log(&self) -> Vec<Value> {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/session/{}/log?after=0", self.session))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 1 << 24).await.unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).expect("event json"))
            .collect()
    }

    async fn pending(&self) -> Vec<Value> {
        let (_, body) = call(
            &self.app,
            Method::GET,
            &format!("/api/session/{}/permission", self.session),
            None,
        )
        .await;
        body.expect("permission list")["data"]
            .as_array()
            .expect("array")
            .clone()
    }

    async fn reply(&self, request_id: &str, decision: &str) -> StatusCode {
        let (status, _) = call(
            &self.app,
            Method::POST,
            &format!(
                "/api/session/{}/permission/{request_id}/reply",
                self.session
            ),
            Some(json!({ "decision": decision, "message": "not now" })),
        )
        .await;
        status
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
    let bytes = to_bytes(response.into_body(), 1 << 24).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).expect("json body"))
    };
    (status, value)
}

fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn usage() -> Usage {
    Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        ..Usage::default()
    }
}

fn text_events(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::StepStart,
        LlmEvent::TextStart,
        LlmEvent::TextDelta {
            delta: text.to_owned(),
        },
        LlmEvent::TextEnd {
            text: text.to_owned(),
        },
        LlmEvent::Finish {
            reason: "stop".to_owned(),
            usage: usage(),
        },
    ]
}

fn text_script(text: &str) -> Script {
    Script::Events(text_events(text))
}

fn expectation() -> Value {
    json!({
        "why": "test tool call",
        "expect_seconds": 5,
        "intervene_after_seconds": 7
    })
}

fn tool_events(call_id: &str, name: &str, input: Value) -> Vec<LlmEvent> {
    let text = input.to_string();
    vec![
        LlmEvent::StepStart,
        LlmEvent::ToolInputStart {
            id: call_id.to_owned(),
            name: name.to_owned(),
        },
        LlmEvent::ToolInputDelta {
            id: call_id.to_owned(),
            delta: text.clone(),
        },
        LlmEvent::ToolInputEnd {
            id: call_id.to_owned(),
            text,
        },
        LlmEvent::ToolCall {
            id: call_id.to_owned(),
            name: name.to_owned(),
            input,
        },
        LlmEvent::Finish {
            reason: "tool_calls".to_owned(),
            usage: usage(),
        },
    ]
}

fn tool_script(call_id: &str, name: &str, input: Value) -> Script {
    Script::Events(tool_events(call_id, name, input))
}

/// Whether any text part of `messages` contains `needle`.
fn requests_mention(messages: &[LlmMessage], needle: &str) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(ContentPart::as_text)
        .any(|text| text.contains(needle))
}

fn assistants(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))
        .collect()
}

fn user_texts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| message.get("text").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

async fn execution_starts(app: &axum::Router, session: &str) -> usize {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/session/{session}/log?after=0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), 1 << 24).await.unwrap();
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| data.contains("\"session.execution.started\""))
        .count()
}

fn tool_states(message: &Value) -> Vec<&Value> {
    message["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter(|part| part["type"] == "tool")
        .map(|part| &part["state"])
        .collect()
}

#[tokio::test]
async fn text_turn_completes_and_log_records_success() {
    let harness = Harness::new(vec![text_script("Hello from the runtime")], None).await;

    let (status, _) = harness.prompt("hi").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 1);
    assert!(
        assistants[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.execution.succeeded")
    );
    assert_eq!(harness.model.calls(), 1);

    let (status, body) = call(&harness.app, Method::GET, "/api/session/active", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({ "data": {} }));

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn tool_turn_reads_a_real_file() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                "read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        None,
    )
    .await;

    let (status, _) = harness.prompt("read the file").await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    let tool_parts: Vec<&Value> = assistants.iter().flat_map(|m| tool_states(m)).collect();
    assert_eq!(tool_parts.len(), 1);
    assert_eq!(tool_parts[0]["status"], "completed");
    assert_eq!(tool_parts[0]["input"]["path"], "hello.txt");

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.tool.success")
    );
    assert_eq!(harness.model.calls(), 2);

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn permission_ask_waits_for_once_reply() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_shell",
                "shell",
                json!({"command": "echo ok", "expectation": expectation()}),
            ),
            text_script("done"),
        ],
        Some(json!([{"action": "shell", "resource": "*", "effect": "ask"}])),
    )
    .await;

    let (status, _) = harness.prompt("run it").await;
    assert_eq!(status, StatusCode::OK);

    let request = wait_for_permission(&harness).await;
    assert_eq!(request["action"], "shell");
    assert_eq!(request["sessionID"], harness.session);
    assert_eq!(request["source"]["type"], "tool");
    assert!(request["id"].as_str().unwrap().starts_with("per_"));

    let id = request["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.reply(&id, "once").await, StatusCode::NO_CONTENT);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let tool_parts: Vec<&Value> = assistants(&messages)
        .iter()
        .flat_map(|message| tool_states(message))
        .collect();
    assert_eq!(tool_parts.len(), 1);
    assert_eq!(tool_parts[0]["status"], "completed");

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.execution.succeeded")
    );
    assert!(harness.pending().await.is_empty());

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn permission_reject_fails_the_tool() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_shell",
                "shell",
                json!({"command": "echo ok", "expectation": expectation()}),
            ),
            text_script("the tool was rejected"),
        ],
        Some(json!([{"action": "shell", "resource": "*", "effect": "ask"}])),
    )
    .await;

    let (status, _) = harness.prompt("run it").await;
    assert_eq!(status, StatusCode::OK);
    let request = wait_for_permission(&harness).await;
    let id = request["id"].as_str().unwrap().to_owned();
    assert_eq!(harness.reply(&id, "reject").await, StatusCode::NO_CONTENT);
    let (status, body) = harness.wait().await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body:?}");

    let messages = harness.messages().await;
    let tool_parts: Vec<&Value> = assistants(&messages)
        .iter()
        .flat_map(|message| tool_states(message))
        .collect();
    assert_eq!(tool_parts.len(), 1);
    assert_eq!(tool_parts[0]["status"], "error");
    assert_eq!(tool_parts[0]["error"]["message"], "not now");

    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.tool.failed")
    );

    harness.runtime.expect("runtime").shutdown().await;
}

#[tokio::test]
async fn interrupt_cancels_a_hanging_stream() {
    let harness = Harness::new(vec![Script::HangAfter(vec![LlmEvent::StepStart])], None).await;

    let (status, _) = harness.prompt("hang").await;
    assert_eq!(status, StatusCode::OK);
    wait_until_active(&harness).await;
    // Interrupting before the step emits its first event leaves no assistant
    // row to mark as aborted; wait for the hanging call to have started.
    let deadline = Instant::now() + Duration::from_secs(5);
    while assistants(&harness.messages().await).is_empty() {
        assert!(
            Instant::now() < deadline,
            "hanging step never created an assistant message"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

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

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants[0]["error"]["type"], "MessageAbortedError");
    let events = harness.log().await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "session.execution.interrupted")
    );

    harness.runtime.expect("runtime").shutdown().await;
}

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

async fn wait_for_permission(harness: &Harness) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pending = harness.pending().await;
        if let Some(request) = pending.first() {
            return request.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for permission request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_until_active(harness: &Harness) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, body) = call(&harness.app, Method::GET, "/api/session/active", None).await;
        if body
            .as_ref()
            .and_then(|body| body["data"].get(&harness.session))
            .is_some()
        {
            return;
        }
        assert!(Instant::now() < deadline, "session never became active");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

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

#[tokio::test]
async fn delete_cancels_a_hanging_provider_call_before_removing_rows() {
    let harness = Harness::new(vec![Script::HangAfter(vec![LlmEvent::StepStart])], None).await;
    let (status, _) = harness.prompt("hang").await;
    assert_eq!(status, StatusCode::OK);
    wait_until_active(&harness).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while assistants(&harness.messages().await).is_empty() {
        assert!(
            Instant::now() < deadline,
            "hanging step never created an assistant message"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let started = Instant::now();
    let (status, body) = call(
        &harness.app,
        Method::DELETE,
        &format!("/api/session/{}", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_none(), "{body:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "DELETE waited on the provider stream for {:?}",
        started.elapsed()
    );
    assert!(
        harness
            .runtime
            .as_ref()
            .expect("runtime")
            .active()
            .is_empty(),
        "the provider call survived DELETE"
    );

    let (status, body) = call(
        &harness.app,
        Method::GET,
        &format!("/api/session/{}", harness.session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.expect("error")["_tag"], "SessionNotFoundError");

    harness.runtime.expect("runtime").shutdown().await;
}
