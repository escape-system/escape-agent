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
use oc_observer::{ObserverConfig, ObserverEngine, ObserverError, ObserverModel, SystemClock};
use oc_runtime::{
    ModelClient, ObserverBinding, RepairChannel, Runtime, RuntimeConfig, StoreGoalStore,
};
use oc_server::observer_host::BusObserverNotifier;
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
pub(crate) enum Script {
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
pub(crate) struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Gate {
    /// Waits until the provider call holding this gate starts.
    pub(crate) async fn wait_entered(&self) {
        self.entered.notified().await;
    }

    pub(crate) fn release(&self) {
        self.release.notify_one();
    }
}

pub(crate) struct ScriptedModel {
    scripts: Mutex<VecDeque<Script>>,
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<LlmMessage>>>,
}

impl ScriptedModel {
    pub(crate) fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(crate) fn requests(&self) -> Vec<Vec<LlmMessage>> {
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

/// Session id every harness creates. Exposed so a test can script an observer
/// verdict that targets it before the harness exists.
pub(crate) const SESSION: &str = "ses_server_runtime";

pub(crate) struct Harness {
    _root: TempDir,
    work: PathBuf,
    pub(crate) session: String,
    pub(crate) app: axum::Router,
    pub(crate) runtime: Option<Arc<Runtime>>,
    pub(crate) model: Arc<ScriptedModel>,
    /// Present only for [`Harness::new_with_observer`].
    pub(crate) observer_model: Option<Arc<ScriptedObserverModel>>,
    pub(crate) store: Arc<Store>,
    config: Arc<ConfigService>,
    catalog: Arc<Catalog>,
}

/// Scripted observer judge: records calls and returns the queued verdict text.
///
/// The observer is wired end-to-end, so a verdict that reaches the engine is
/// applied by the runtime exactly as in production (a `repost` becomes a
/// synthetic message and a new execution). A test asserts `calls() == 0` to
/// prove the engine's user-stop guard short-circuited before the judge.
pub(crate) struct ScriptedObserverModel {
    verdicts: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
}

impl ScriptedObserverModel {
    pub(crate) fn new(verdicts: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            verdicts: Mutex::new(verdicts.into()),
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ObserverModel for ScriptedObserverModel {
    fn judge<'a>(
        &'a self,
        _input: &'a str,
        _cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .verdicts
            .lock()
            .expect("verdicts")
            .pop_front()
            .unwrap_or_else(|| r#"{"action":"none","reason":"script exhausted"}"#.to_owned());
        Box::pin(async move { Ok(next) })
    }
}

impl Harness {
    pub(crate) async fn new(scripts: Vec<Script>, session_permissions: Option<Value>) -> Self {
        Self::build(scripts, session_permissions, None).await
    }

    /// Like [`Harness::new`], but additionally attaches a real
    /// [`ObserverEngine`] to the runtime and the state, the way
    /// `oc_cli::bootstrap::attach_runtime` does in production.
    ///
    /// The judge is a local double returning `verdicts` in order and counting
    /// calls; the executor is the runtime's real [`RepairChannel`], so any
    /// verdict that reaches the engine is applied for real. `tick_ms` is set
    /// far above the test duration and `min_interval_ms`/`max_calls_per_min`
    /// are disabled, so the only thing that can stop a model call is the
    /// engine's user-stop guard.
    pub(crate) async fn new_with_observer(
        scripts: Vec<Script>,
        session_permissions: Option<Value>,
        verdicts: Vec<String>,
    ) -> Self {
        Self::build(scripts, session_permissions, Some(verdicts)).await
    }

    // One linear harness setup: temp tree, config, catalog, store, observer.
    #[allow(clippy::too_many_lines)]
    async fn build(
        scripts: Vec<Script>,
        session_permissions: Option<Value>,
        observer_verdicts: Option<Vec<String>>,
    ) -> Self {
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
        let session = SESSION.to_owned();
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
        let mut runtime = Runtime::new_with_model_client(
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
        );

        let mut observer_model = None;
        let mut observer_engine = None;
        if let Some(verdicts) = observer_verdicts {
            let judge = ScriptedObserverModel::new(verdicts);
            let (executor, repairs) = RepairChannel::new();
            let directory: Arc<str> = Arc::from(state.directory().to_string_lossy().as_ref());
            let engine = Arc::new(ObserverEngine::new(
                ObserverConfig {
                    model: Some("test/alpha".to_owned()),
                    // The ticker must not fire during the test: the forced
                    // `POST /observer/check` below is the single engine run,
                    // which keeps the assertion deterministic.
                    tick_ms: 60_000,
                    // No coalescing/rate limiting, so a broken guard would
                    // certainly call the judge instead of a `Skipped` run.
                    min_interval_ms: 0,
                    max_calls_per_min: 100,
                    ..ObserverConfig::default()
                },
                Arc::new(SystemClock),
                Arc::clone(&judge) as Arc<dyn ObserverModel>,
                executor,
                Arc::new(BusObserverNotifier::new(
                    state.bus().clone(),
                    Arc::clone(&store),
                    directory,
                )),
            ));
            runtime = runtime.with_observer(ObserverBinding {
                engine: Arc::clone(&engine),
                goals: Arc::new(StoreGoalStore::new(Arc::clone(&store))),
                repairs,
            });
            observer_model = Some(judge);
            observer_engine = Some(engine);
        }
        runtime.start().await.expect("runtime starts");
        let runtime = Arc::new(runtime);

        let mut state = state
            .with_runtime(Arc::clone(&runtime))
            .with_permissions(broker);
        if let Some(engine) = observer_engine {
            state = state.with_observer_engine(engine);
        }
        let app = router(state);
        Self {
            _root: root,
            work,
            session,
            app,
            runtime: Some(runtime),
            model,
            observer_model,
            store,
            config,
            catalog,
        }
    }

    pub(crate) async fn without_runtime() -> (Self, axum::Router) {
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

    pub(crate) async fn prompt(&self, text: &str) -> (StatusCode, Option<Value>) {
        call(
            &self.app,
            Method::POST,
            &format!("/api/session/{}/prompt", self.session),
            Some(json!({ "text": text })),
        )
        .await
    }

    pub(crate) async fn wait(&self) -> (StatusCode, Option<Value>) {
        call(
            &self.app,
            Method::POST,
            &format!("/api/experimental/session/{}/wait?timeout=10", self.session),
            None,
        )
        .await
    }

    pub(crate) async fn messages(&self) -> Vec<Value> {
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

    pub(crate) async fn log(&self) -> Vec<Value> {
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

    pub(crate) async fn pending(&self) -> Vec<Value> {
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

    pub(crate) async fn reply(&self, request_id: &str, decision: &str) -> StatusCode {
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

pub(crate) async fn call(
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

pub(crate) fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(crate) fn usage() -> Usage {
    Usage {
        input_tokens: Some(10),
        output_tokens: Some(5),
        total_tokens: Some(15),
        ..Usage::default()
    }
}

pub(crate) fn text_events(text: &str) -> Vec<LlmEvent> {
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

pub(crate) fn text_script(text: &str) -> Script {
    Script::Events(text_events(text))
}

pub(crate) fn expectation() -> Value {
    json!({
        "why": "test tool call",
        "expect_seconds": 5,
        "intervene_after_seconds": 7
    })
}

pub(crate) fn tool_events(call_id: &str, name: &str, input: Value) -> Vec<LlmEvent> {
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

pub(crate) fn tool_script(call_id: &str, name: &str, input: Value) -> Script {
    Script::Events(tool_events(call_id, name, input))
}

/// Whether any text part of `messages` contains `needle`.
pub(crate) fn requests_mention(messages: &[LlmMessage], needle: &str) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(ContentPart::as_text)
        .any(|text| text.contains(needle))
}

pub(crate) fn assistants(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))
        .collect()
}

pub(crate) fn user_texts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|message| message.get("text").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

pub(crate) async fn execution_starts(app: &axum::Router, session: &str) -> usize {
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

pub(crate) fn tool_states(message: &Value) -> Vec<&Value> {
    message["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter(|part| part["type"] == "tool")
        .map(|part| &part["state"])
        .collect()
}

pub(crate) async fn wait_for_permission(harness: &Harness) -> Value {
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

pub(crate) async fn wait_until_active(harness: &Harness) {
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
