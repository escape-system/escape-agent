//! Offline end-to-end tests for the session actor and turn loop.
//!
//! Every test uses a temp SQLite database, a temp config tree and a scripted
//! model client; no network credentials or HTTP servers are involved.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{
    Catalog, LlmClient, LlmError, LlmEvent, LlmRequest, LlmStream, Role, SharedLlmClient, Usage,
};
use oc_memory::{
    BoxFuture as MemoryFuture, MemoryError, MemoryService, NewMemoryInput, ScribeModel,
};
use oc_runtime::{
    EventSink, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler, Runtime,
    RuntimeConfig, SubagentTools, ToolProvider,
};
use oc_store::{
    Delivery, MemoryQuery, MessageQuery, NewCredential, NewInbox, NewMessage, NewSession,
    NewSessionTree, Order, SessionQuery, Store,
};
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::{EventEnvelope, MessageId};
use serde_json::{Value, json};
use tempfile::TempDir;

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
    Delayed {
        events: Vec<LlmEvent>,
        delay: Duration,
    },
}

struct ScriptedModel {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<LlmRequest>>,
    calls: AtomicUsize,
}

impl ScriptedModel {
    fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().expect("requests").clone()
    }
}

impl ModelClient for ScriptedModel {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        self.requests.lock().expect("requests").push(request);
        self.calls.fetch_add(1, Ordering::SeqCst);
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
                Script::Delayed { events, delay } => {
                    tokio::time::sleep(delay).await;
                    Ok(stream::iter(events).boxed())
                }
            }
        })
    }
}

#[derive(Clone, Default)]
struct TestSink {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl EventSink for TestSink {
    fn publish(&self, event: EventEnvelope) {
        self.events.lock().expect("events").push(event);
    }
}

impl TestSink {
    fn kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .map(|event| event.kind.clone())
            .collect()
    }

    fn has(&self, kind: &str) -> bool {
        self.events
            .lock()
            .expect("events")
            .iter()
            .any(|event| event.kind == kind)
    }

    fn count(&self, kind: &str) -> usize {
        self.events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    fn data(&self, kind: &str) -> Vec<Value> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.data.clone())
            .collect()
    }
}

struct AllowAll;

impl PermissionHandler for AllowAll {
    fn resolve(&self, _request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        Box::pin(async { PermissionDecision::Allow })
    }
}

struct DenyAll;

impl PermissionHandler for DenyAll {
    fn resolve(&self, _request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        Box::pin(async {
            PermissionDecision::Deny {
                message: "no tools for you".to_owned(),
            }
        })
    }
}

/// Allows everything except `read`, which stays pending forever so a test can
/// observe whether the runtime drops the ask when the session is interrupted.
#[derive(Default)]
struct PendingReadAsk {
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

struct PendingAskGuard {
    pending: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

impl Drop for PendingAskGuard {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl PendingReadAsk {
    fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl PermissionHandler for PendingReadAsk {
    fn resolve(&self, request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        if request.action != "read" {
            return Box::pin(async { PermissionDecision::Allow });
        }
        self.pending.fetch_add(1, Ordering::SeqCst);
        let guard = PendingAskGuard {
            pending: Arc::clone(&self.pending),
            dropped: Arc::clone(&self.dropped),
        };
        Box::pin(async move {
            let _guard = guard;
            std::future::pending::<PermissionDecision>().await
        })
    }
}

struct FakeScribe {
    response: String,
    prompts: Mutex<Vec<String>>,
}

impl FakeScribe {
    fn new(response: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            response: response.into(),
            prompts: Mutex::new(Vec::new()),
        })
    }
}

impl ScribeModel for FakeScribe {
    fn complete<'a>(&'a self, prompt: &'a str) -> MemoryFuture<'a, Result<String, MemoryError>> {
        self.prompts
            .lock()
            .expect("prompts")
            .push(prompt.to_owned());
        let response = self.response.clone();
        Box::pin(async move { Ok(response) })
    }
}

struct Harness {
    _root: TempDir,
    work: PathBuf,
    runtime: Runtime,
    config: RuntimeConfig,
    store: Arc<Store>,
    sink: TestSink,
    model: Arc<ScriptedModel>,
    session: String,
}

impl Harness {
    async fn new(
        scripts: Vec<Script>,
        permissions: Arc<dyn PermissionHandler>,
        config_content: Value,
        session_permissions: Option<Value>,
    ) -> Self {
        Self::new_with_setup(
            scripts,
            permissions,
            config_content,
            session_permissions,
            |_| {},
        )
        .await
    }

    async fn new_with_setup(
        scripts: Vec<Script>,
        permissions: Arc<dyn PermissionHandler>,
        config_content: Value,
        session_permissions: Option<Value>,
        setup: impl FnOnce(&Path),
    ) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        std::fs::write(work.join("hello.txt"), "hello world\n").expect("file");
        setup(&work);

        let config = ConfigService::load(
            LoadOptions::new(&work)
                .with_env("HOME", path(root.path()))
                .with_env("USERPROFILE", path(root.path()))
                .with_env("XDG_CONFIG_HOME", path(&root.path().join("config")))
                .with_env("ProgramData", path(&root.path().join("programdata")))
                .with_env("OPENCODE_CONFIG", path(&root.path().join("missing.json")))
                .with_env("OPENCODE_CONFIG_CONTENT", config_content.to_string()),
        )
        .expect("config loads");

        let store = Arc::new(
            Store::open(root.path().join("db.sqlite"))
                .await
                .expect("store opens"),
        );
        let session = "ses_runtime_test".to_owned();
        store
            .create_session(NewSession {
                id: session.clone(),
                parent_id: None,
                project_id: "prj_runtime_test".to_owned(),
                directory: path(&work),
                title: Some("runtime test".to_owned()),
                agent: Some("build".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: session_permissions,
                time_created: oc_types::now_millis(),
            })
            .await
            .expect("session created");

        let catalog = Arc::new(Catalog::from_snapshot(CATALOG).expect("catalog"));
        let client = Arc::new(SharedLlmClient::new(LlmClient::new().expect("client")));
        let sink = TestSink::default();
        let model = ScriptedModel::new(scripts);
        let config = RuntimeConfig {
            store: Arc::clone(&store),
            config: Arc::new(config),
            catalog,
            client,
            tools: Arc::new(ToolRegistry::with_defaults()),
            sink: Arc::new(sink.clone()),
            permissions,
        };
        let runtime = Runtime::new_with_model_client(config.clone(), Arc::clone(&model) as _);
        Self {
            _root: root,
            work,
            runtime,
            config,
            store,
            sink,
            model,
            session,
        }
    }

    async fn enqueue(&self, text: &str) -> String {
        let id = MessageId::new().into_inner();
        self.store
            .enqueue(NewInbox {
                id: id.clone(),
                session_id: self.session.clone(),
                kind: "user".to_owned(),
                payload: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
                delivery: Delivery::Steer,
            })
            .await
            .expect("enqueued");
        id
    }

    async fn prompt(&self, text: &str) {
        self.enqueue(text).await;
        self.runtime.notify(&self.session).await.expect("notify");
        self.runtime
            .wait_idle(&self.session, Duration::from_secs(10))
            .await
            .expect("idle");
    }

    fn restart(&self) -> Runtime {
        Runtime::new_with_model_client(self.config.clone(), Arc::clone(&self.model) as _)
    }

    fn attach_memory(mut self, memory: Arc<oc_memory::MemoryService>) -> Self {
        self.runtime = self.runtime.clone().with_memory(memory);
        self
    }

    fn attach_scribe(mut self, scribe: Arc<dyn oc_memory::ScribeModel>) -> Self {
        self.runtime = self.runtime.clone().with_scribe(scribe);
        self
    }

    async fn messages(&self) -> Vec<Value> {
        let mut cursor = None;
        let mut out = Vec::new();
        loop {
            let page = self
                .store
                .list_messages(MessageQuery {
                    session_id: self.session.clone(),
                    limit: 200,
                    order: Order::Asc,
                    cursor,
                    kind: None,
                })
                .await
                .expect("messages");
            out.extend(page.items);
            match page.cursor.next {
                Some(next) => cursor = Some(next),
                None => return out,
            }
        }
    }

    async fn wait_for_event(&self, kind: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.sink.has(kind) {
            assert!(Instant::now() < deadline, "timed out waiting for {kind}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
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

fn text_script(text: &str) -> Script {
    Script::Events(vec![
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
    ])
}

fn delayed_text_script(text: &str, delay: Duration) -> Script {
    match text_script(text) {
        Script::Events(events) => Script::Delayed { events, delay },
        _ => unreachable!("text_script always returns events"),
    }
}

fn tool_script(call_id: &str, input: Value) -> Script {
    named_tool_script("read", call_id, input)
}

fn named_tool_script(name: &str, call_id: &str, input: Value) -> Script {
    let text = input.to_string();
    Script::Events(vec![
        LlmEvent::StepStart,
        LlmEvent::TextStart,
        LlmEvent::TextDelta {
            delta: "checking".to_owned(),
        },
        LlmEvent::TextEnd {
            text: "checking".to_owned(),
        },
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
    ])
}

async fn seed_memory(
    memory: &MemoryService,
    text: &str,
    scope: &str,
    scope_id: Option<&str>,
    importance: u8,
) {
    memory
        .write(NewMemoryInput {
            text: text.to_owned(),
            scope: Some(scope.to_owned()),
            scope_id: scope_id.map(str::to_owned),
            importance: Some(importance),
            ..NewMemoryInput::default()
        })
        .await
        .expect("memory seeded");
}

async fn all_memories(store: &Store) -> Vec<Value> {
    store
        .list_memories(MemoryQuery {
            scopes: Vec::new(),
            query: None,
            kinds: Vec::new(),
            tags: Vec::new(),
            limit: 50,
            pinned_min_importance: None,
        })
        .await
        .expect("memories")
}

fn request_tool_result(request: &LlmRequest, index: usize) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .filter_map(|message| message.content.first())
        .filter_map(|part| match part {
            oc_llm::ContentPart::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .nth(index)
        .unwrap_or_default()
}

fn expectation() -> Value {
    json!({
        "why": "read the greeting file",
        "expect_seconds": 5,
        "intervene_after_seconds": 7
    })
}

fn assistants(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))
        .collect()
}

#[test]
fn runtime_handle_is_clone_send_sync() {
    fn assert_handle<T: Clone + Send + Sync>() {}
    assert_handle::<Runtime>();
    assert_handle::<RuntimeConfig>();
}

#[tokio::test]
async fn text_only_turn_persists_and_publishes() {
    let harness = Harness::new(
        vec![text_script("Hello from the model")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    assert!(harness.sink.has("session.execution.started"));
    assert!(harness.sink.has("session.execution.succeeded"));
    assert!(harness.sink.has("session.step.started"));
    assert!(harness.sink.has("session.step.streamed"));
    assert!(harness.sink.has("session.step.ended"));
    assert_eq!(harness.sink.count("session.text.delta"), 1);
    assert!(harness.sink.has("session.text.ended"));
    assert!(harness.sink.has("session.usage.updated"));

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0]["content"][0]["type"], "text");
    assert_eq!(assistants[0]["content"][0]["text"], "Hello from the model");
    assert_eq!(assistants[0]["finish"], "stop");
    assert!(assistants[0]["time"]["completed"].is_i64());

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].system.len(), 2);
    assert!(requests[0].system[1].contains("<env>"));
    assert!(!requests[0].tools.is_empty());
    let read = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "read")
        .expect("read tool");
    let required = read.input_schema["required"].as_array().unwrap();
    assert!(required.iter().any(|value| value == "expectation"));
}

#[tokio::test]
async fn stored_credentials_reach_the_provider_request() {
    let harness = Harness::new(
        vec![
            text_script("keyed"),
            text_script("unkeyed"),
            text_script("oauth"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;

    harness
        .store
        .create_credential(NewCredential {
            id: Some("cred_runtime".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "key", "key": "sk-stored"}),
            active: Some(true),
        })
        .await
        .expect("credential");
    harness.prompt("hi").await;
    assert_eq!(
        harness.model.requests()[0].options.api_key.as_deref(),
        Some("sk-stored")
    );

    harness
        .store
        .delete_credential("cred_runtime")
        .await
        .expect("delete");
    harness.prompt("again").await;
    assert_eq!(harness.model.requests()[1].options.api_key, None);

    harness
        .store
        .create_credential(NewCredential {
            id: Some("cred_oauth".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "oauth", "access_token": "oauth-token"}),
            active: Some(true),
        })
        .await
        .expect("oauth credential");
    harness.prompt("oauth please").await;
    assert_eq!(
        harness.model.requests()[2].options.api_key.as_deref(),
        Some("oauth-token")
    );
}

#[tokio::test]
async fn tool_call_runs_then_second_step_answers() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read the file").await;

    assert!(harness.sink.has("session.tool.called"));
    assert!(harness.sink.has("session.tool.success"));
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 2);

    let requests = harness.model.requests();
    let second = &requests[1];
    let result = second
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("tool result");
    let text = match &result.content[0] {
        oc_llm::ContentPart::ToolResult { content, .. } => content,
        other => panic!("unexpected part {other:?}"),
    };
    assert!(text.contains("hello world"), "got {text:?}");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0]["content"][1]["state"]["status"], "completed");
    assert_eq!(
        assistants[0]["content"][1]["state"]["input"]["path"],
        "hello.txt"
    );
}

#[tokio::test]
async fn tool_error_continues_to_a_final_answer() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "missing.txt", "expectation": expectation()}),
            ),
            text_script("I could not read it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read missing").await;

    assert!(harness.sink.has("session.tool.failed"));
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 2);
    let requests = harness.model.requests();
    let result = requests[1]
        .messages
        .iter()
        .find(|message| message.role == Role::Tool)
        .expect("tool result");
    match &result.content[0] {
        oc_llm::ContentPart::ToolResult { is_error, .. } => assert!(*is_error),
        other => panic!("unexpected part {other:?}"),
    }
}

#[tokio::test]
async fn permission_deny_becomes_a_tool_error() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("permission was denied"),
        ],
        Arc::new(DenyAll),
        json!({}),
        Some(json!([{"action": "read", "resource": "*", "effect": "ask"}])),
    )
    .await;
    harness.prompt("read anyway").await;

    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["error"]["type"], "PermissionDeniedError");
    assert_eq!(failures[0]["error"]["message"], "no tools for you");
    assert_eq!(failures[0]["executed"], false);
    assert!(!harness.sink.has("session.tool.success"));
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn interrupt_mid_stream_runs_the_finalizer() {
    let harness = Harness::new(
        vec![Script::HangAfter(vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart,
            LlmEvent::TextDelta {
                delta: "partial".to_owned(),
            },
        ])],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.enqueue("long answer").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness.wait_for_event("session.text.delta").await;

    assert!(
        harness
            .runtime
            .interrupt(&harness.session)
            .await
            .expect("interrupt")
    );
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert!(harness.sink.has("session.text.ended"));
    assert!(harness.sink.has("session.step.failed"));
    assert!(harness.sink.has("session.execution.interrupted"));
    assert!(!harness.sink.has("session.execution.succeeded"));
    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted[0]["reason"], "user");

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    assert_eq!(assistants[0]["error"]["type"], "MessageAbortedError");
    assert_eq!(assistants[0]["finish"], "error");
    assert!(assistants[0]["time"]["completed"].is_i64());
}

#[tokio::test]
async fn max_steps_makes_one_final_tool_less_call() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            tool_script(
                "call_2",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            tool_script(
                "call_3",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
        ],
        Arc::new(AllowAll),
        json!({"agents": {"build": {"steps": 2}}}),
        None,
    )
    .await;
    harness.prompt("keep going").await;

    assert_eq!(harness.model.calls(), 3);
    let requests = harness.model.requests();
    assert!(requests[2].tools.is_empty());
    assert_eq!(requests[2].options.tool_choice.as_deref(), Some("none"));
    assert_eq!(harness.sink.count("session.tool.success"), 2);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn two_prompts_serialize() {
    let harness = Harness::new(
        vec![text_script("first"), text_script("second")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.enqueue("one").await;
    harness.enqueue("two").await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert_eq!(harness.sink.count("session.execution.started"), 2);
    assert_eq!(harness.sink.count("session.execution.succeeded"), 2);
    assert_eq!(harness.model.calls(), 2);
    let messages = harness.messages().await;
    assert_eq!(assistants(&messages).len(), 2);
    let kinds = harness.sink.kinds();
    let starts: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| *kind == "session.execution.started")
        .map(|(index, _)| index)
        .collect();
    let done: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| *kind == "session.execution.succeeded")
        .map(|(index, _)| index)
        .collect();
    assert!(starts[0] < done[0]);
    assert!(done[0] < starts[1]);
    assert!(starts[1] < done[1]);
}

#[tokio::test]
async fn reopened_runtime_sees_prior_context() {
    let harness = Harness::new(
        vec![text_script("one"), text_script("two")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("first prompt").await;
    harness.runtime.shutdown().await;

    let reopened = harness.restart();
    harness.enqueue("second prompt").await;
    reopened.notify(&harness.session).await.expect("notify");
    reopened
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2);
    let texts: Vec<String> = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(oc_llm::ContentPart::as_text)
        .map(str::to_owned)
        .collect();
    assert!(texts.iter().any(|text| text == "first prompt"));
    assert!(texts.iter().any(|text| text == "one"));
    assert!(texts.iter().any(|text| text == "second prompt"));
}

#[tokio::test]
async fn expectation_is_injected_and_stripped() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_1",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("done"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("read").await;

    let requests = harness.model.requests();
    let schema = &requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "read")
        .expect("read")
        .input_schema;
    assert_eq!(
        schema["properties"]["expectation"]["required"],
        json!(["why", "expect_seconds", "intervene_after_seconds"])
    );
    let called = harness.sink.data("session.tool.called");
    assert!(called[0]["input"].get("expectation").is_none());

    let messages = harness.messages().await;
    let assistants = assistants(&messages);
    let input = &assistants[0]["content"][1]["state"]["input"];
    assert_eq!(input["path"], "hello.txt");
    assert!(input.get("expectation").is_none());
}

#[tokio::test]
async fn host_materialized_prompt_is_not_duplicated() {
    let harness = Harness::new(
        vec![text_script("answered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let id = harness.enqueue("host wrote me").await;
    harness
        .store
        .append_message(NewMessage {
            id: id.clone(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            data: json!({"text": "host wrote me", "time": {"created": oc_types::now_millis()}}),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("host message");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let messages = harness.messages().await;
    let users: Vec<&Value> = messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("user"))
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["id"], id);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn oversized_context_compacts_before_the_request() {
    let harness = Harness::new(
        vec![
            text_script("## Objective\nhandoff summary"),
            text_script("answer from the summary"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    // The fixture context window is 100k, so the 85k-token ceiling needs a
    // transcript above roughly 340k characters.
    let huge = "a".repeat(360_000);
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": huge}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert_eq!(harness.model.calls(), 2);
    let requests = harness.model.requests();
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].options.tool_choice.as_deref(), Some("none"));
    let summary_seen = requests[1].messages.iter().any(|message| {
        message.content.iter().any(|part| {
            part.as_text()
                .is_some_and(|text| text.contains("handoff summary"))
        })
    });
    assert!(summary_seen);

    let messages = harness.messages().await;
    let compaction = messages
        .iter()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
        .expect("compaction row");
    assert_eq!(compaction["status"], "completed");
    assert!(compaction["summary"].as_str().unwrap().contains("handoff"));
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn crash_recovery_repairs_rows_and_wakes_inbox() {
    let harness = Harness::new(
        vec![text_script("recovered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_message(NewMessage {
            id: "msg_stale".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "build",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_stale",
                    "name": "read",
                    "state": {"status": "running", "input": {"path": "hello.txt"}},
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("stale assistant");
    harness.enqueue("after the crash").await;

    harness.runtime.start().await.expect("start");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let stale = harness
        .store
        .get_message(&harness.session, "msg_stale")
        .await
        .expect("read stale")
        .expect("stale exists");
    assert_eq!(stale["finish"], "error");
    assert_eq!(stale["error"]["type"], "MessageAbortedError");
    assert!(stale["time"]["completed"].is_i64());
    assert_eq!(stale["content"][0]["state"]["status"], "error");
    assert_eq!(
        stale["content"][0]["state"]["error"]["type"],
        "ToolAbortedError"
    );

    let interrupted = harness.sink.data("session.execution.interrupted");
    assert_eq!(interrupted[0]["reason"], "shutdown");
    assert!(harness.sink.has("session.execution.succeeded"));
    assert_eq!(harness.model.calls(), 1);
    assert!(harness.work.join("hello.txt").exists());
}

// ---------------------------------------------------------------------------
// Observer wiring
// ---------------------------------------------------------------------------

use oc_observer::{
    FlightStrip, GoalSnapshot, GoalStatus, GoalStore, ObserverConfig, ObserverEngine,
    ObserverError, ObserverModel, ObserverNotice, ObserverNotifier, RepairExecutor, RepairOutcome,
    SystemClock,
};
use oc_runtime::{ObserverBinding, RepairChannel, RepairCommand, StoreGoalStore};
use oc_store::NewGoal;
use tokio::sync::mpsc;

enum ObserverReply {
    Text(String),
    Error(String),
}

struct FixedObserverModel {
    reply: ObserverReply,
    calls: AtomicUsize,
}

impl FixedObserverModel {
    fn text(text: &Value) -> Arc<Self> {
        Arc::new(Self {
            reply: ObserverReply::Text(text.to_string()),
            calls: AtomicUsize::new(0),
        })
    }

    fn error(message: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: ObserverReply::Error(message.to_owned()),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ObserverModel for FixedObserverModel {
    fn judge<'a>(
        &'a self,
        _input: &'a str,
        _cfg: &'a ObserverConfig,
    ) -> oc_observer::BoxFuture<'a, Result<String, ObserverError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = match &self.reply {
            ObserverReply::Text(text) => Ok(text.clone()),
            ObserverReply::Error(message) => Err(ObserverError::Model(message.clone())),
        };
        Box::pin(async move { reply })
    }
}

#[derive(Default)]
struct RecordingNotifier {
    notices: Mutex<Vec<ObserverNotice>>,
}

impl ObserverNotifier for RecordingNotifier {
    fn notify(&self, event: ObserverNotice) {
        self.notices.lock().expect("notices").push(event);
    }
}

impl RecordingNotifier {
    fn has_unavailable(&self) -> bool {
        self.notices
            .lock()
            .expect("notices")
            .iter()
            .any(|notice| matches!(notice, ObserverNotice::Unavailable { .. }))
    }
}

struct NeverExecutor;

impl RepairExecutor for NeverExecutor {
    fn apply<'a>(
        &'a self,
        _verdict: &'a oc_observer::ObserverVerdict,
    ) -> oc_observer::BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        Box::pin(async {
            Err(ObserverError::Executor(
                "this test does not execute repairs".to_owned(),
            ))
        })
    }
}

fn observer_config() -> ObserverConfig {
    ObserverConfig {
        tick_ms: 20,
        min_interval_ms: 0,
        ..ObserverConfig::default()
    }
}

fn observer_engine(
    model: Arc<FixedObserverModel>,
    notifier: Arc<RecordingNotifier>,
    executor: Arc<dyn RepairExecutor>,
) -> Arc<ObserverEngine> {
    Arc::new(ObserverEngine::new(
        observer_config(),
        Arc::new(SystemClock),
        model,
        executor,
        notifier,
    ))
}

fn goals(harness: &Harness) -> Arc<dyn oc_observer::GoalStore> {
    Arc::new(StoreGoalStore::new(Arc::clone(&harness.store)))
}

async fn wait_for_observer_inbox(harness: &Harness) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let inbox = harness
            .store
            .list_inbox(&harness.session)
            .await
            .expect("inbox");
        if let Some(item) = inbox.iter().find(|item| {
            item.get("type").and_then(Value::as_str) == Some("synthetic")
                && item["payload"]["metadata"]["observer"] == json!(true)
        }) {
            return item.clone();
        }
        let messages = harness.messages().await;
        if let Some(message) = messages.iter().find(|message| {
            message.get("type").and_then(Value::as_str) == Some("synthetic")
                && message["metadata"]["observer"] == json!(true)
        }) {
            return message.clone();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for an observer synthetic message"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn observer_records_tool_strip() {
    let harness = Harness::new(
        vec![
            tool_script(
                "call_obs",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("done"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let notifier = Arc::new(RecordingNotifier::default());
    let (executor, repairs) = RepairChannel::new();
    let engine = observer_engine(
        FixedObserverModel::text(&json!({"action": "none"})),
        Arc::clone(&notifier),
        executor,
    );
    let runtime = harness.restart().with_observer(ObserverBinding {
        engine: Arc::clone(&engine),
        goals: goals(&harness),
        repairs,
    });
    runtime.start().await.expect("start");

    harness.enqueue("go").await;
    runtime.notify(&harness.session).await.expect("notify");
    runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let recent = engine.registry().recent(&harness.session, 10);
    assert_eq!(recent.len(), 1);
    let strip = &recent[0];
    assert_eq!(strip.tool, "read");
    assert_eq!(strip.root_id, harness.session);
    assert_eq!(strip.session_id, harness.session);
    assert_eq!(strip.tool_call_id, "call_obs");
    assert_eq!(strip.expect_s, 5);
    assert_eq!(strip.intervene_s, 7);
    assert_eq!(strip.outcome.as_deref(), Some("ok"));
    assert_eq!(strip.state, oc_observer::StripState::Ok);
    assert!(strip.retryable);
    assert_eq!(strip.why, "read the greeting file");
}

#[tokio::test]
async fn overdue_strip_nudges_a_synthetic_message() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let notifier = Arc::new(RecordingNotifier::default());
    let (executor, repairs) = RepairChannel::new();
    let engine = observer_engine(
        FixedObserverModel::text(&json!({
            "action": "nudge",
            "target": harness.session,
            "reason": "the shell call is overdue",
            "message": "check the overdue shell call"
        })),
        Arc::clone(&notifier),
        executor,
    );
    let runtime = harness.restart().with_observer(ObserverBinding {
        engine: Arc::clone(&engine),
        goals: goals(&harness),
        repairs,
    });
    runtime.start().await.expect("start");

    let strip = FlightStrip {
        id: oc_types::ObservationId::new().into_inner(),
        root_id: harness.session.clone(),
        session_id: harness.session.clone(),
        agent: "build".to_owned(),
        tool_call_id: "call_overdue".to_owned(),
        tool: "shell".to_owned(),
        why: "run a command and return its result".to_owned(),
        expect_s: 1,
        intervene_s: 0,
        started_ms: oc_types::now_millis(),
        ..FlightStrip::default()
    };
    engine.observe_start(strip);

    runtime.notify(&harness.session).await.expect("notify");
    let item = wait_for_observer_inbox(&harness).await;
    let text = item["payload"]["text"]
        .as_str()
        .or_else(|| item["text"].as_str());
    assert_eq!(text, Some("check the overdue shell call"));
    let target = item["payload"]["metadata"]["target"]
        .as_str()
        .or_else(|| item["metadata"]["target"].as_str());
    assert_eq!(target, Some(harness.session.as_str()));
}

#[tokio::test]
async fn failed_execution_reaches_the_observer() {
    let harness = Harness::new(
        vec![Script::Events(vec![
            LlmEvent::StepStart,
            LlmEvent::ProviderError {
                message: "provider exploded".to_owned(),
            },
        ])],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let notifier = Arc::new(RecordingNotifier::default());
    let (executor, repairs) = RepairChannel::new();
    let engine = observer_engine(
        FixedObserverModel::error("observer model offline"),
        Arc::clone(&notifier),
        executor,
    );
    let runtime = harness.restart().with_observer(ObserverBinding {
        engine,
        goals: goals(&harness),
        repairs,
    });
    runtime.start().await.expect("start");

    harness.enqueue("boom").await;
    runtime.notify(&harness.session).await.expect("notify");
    runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");
    assert!(harness.sink.has("session.execution.failed"));

    let deadline = Instant::now() + Duration::from_secs(5);
    while !notifier.has_unavailable() {
        assert!(
            Instant::now() < deadline,
            "observer did not run after the execution failure"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn repair_worker_interrupts_the_execution() {
    let harness = Harness::new(
        vec![Script::HangAfter(vec![
            LlmEvent::StepStart,
            LlmEvent::TextStart,
            LlmEvent::TextDelta {
                delta: "partial".to_owned(),
            },
        ])],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let notifier = Arc::new(RecordingNotifier::default());
    let engine = observer_engine(
        FixedObserverModel::text(&json!({"action": "none"})),
        notifier,
        Arc::new(NeverExecutor),
    );
    let (sender, repairs) = mpsc::unbounded_channel();
    let runtime = harness.restart().with_observer(ObserverBinding {
        engine,
        goals: goals(&harness),
        repairs,
    });
    runtime.start().await.expect("start");

    harness.enqueue("long answer").await;
    runtime.notify(&harness.session).await.expect("notify");
    harness.wait_for_event("session.text.delta").await;
    sender
        .send(RepairCommand::Interrupt {
            session_id: harness.session.clone(),
        })
        .expect("send interrupt");
    runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    assert!(harness.sink.has("session.execution.interrupted"));
    assert!(!harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn goal_store_round_trips_a_snapshot() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    harness
        .store
        .set_goal(NewGoal {
            id: "gol_test".to_owned(),
            root_id: harness.session.clone(),
            objective: "ship the observer".to_owned(),
            criteria: vec!["strips settle".to_owned(), "goals persist".to_owned()],
            model: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("goal");

    let adapter = StoreGoalStore::new(Arc::clone(&harness.store));
    let snapshot: GoalSnapshot = adapter
        .load(&harness.session)
        .await
        .expect("load")
        .expect("goal exists");
    assert_eq!(snapshot.id, "gol_test");
    assert_eq!(snapshot.objective, "ship the observer");
    assert_eq!(snapshot.total, 2);
    assert_eq!(snapshot.done, 0);
    assert_eq!(snapshot.criteria.len(), 2);
    assert_eq!(snapshot.status, GoalStatus::Active);

    let mut done = snapshot;
    done.status = GoalStatus::Done;
    done.evidence = vec!["msg_one".to_owned(), "msg_two".to_owned()];
    done.last_progress_ms = oc_types::now_millis();
    adapter.save(&done).await.expect("save");

    let raw = harness
        .store
        .get_goal(&harness.session)
        .await
        .expect("raw")
        .expect("goal exists");
    assert_eq!(raw["status"], "done");
    assert_eq!(raw["evidence"], json!(["msg_one", "msg_two"]));
    let reloaded = adapter
        .load(&harness.session)
        .await
        .expect("reload")
        .expect("goal exists");
    assert_eq!(reloaded.status, GoalStatus::Done);
    assert_eq!(reloaded.done, 2);
}

fn write_skill(
    work: &Path,
    id: &str,
    name: &str,
    description: Option<&str>,
    autoinvoke: Option<bool>,
) {
    let directory = work.join(".opencode").join("skill").join(id);
    std::fs::create_dir_all(&directory).expect("skill dir");
    let mut content = format!("---\nname: {name}\n");
    if let Some(description) = description {
        let _ = writeln!(content, "description: {description}");
    }
    if let Some(autoinvoke) = autoinvoke {
        let _ = write!(content, "metadata:\n  opencode/autoinvoke: {autoinvoke}\n");
    }
    content.push_str("---\nSkill body.\n");
    std::fs::write(directory.join("SKILL.md"), content).expect("skill file");
}

async fn wait_for_memories(store: &Store, source: &str) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rows: Vec<Value> = all_memories(store)
            .await
            .into_iter()
            .filter(|row| row["source"].as_str() == Some(source))
            .collect();
        if !rows.is_empty() || Instant::now() >= deadline {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn memory_block_appears_only_when_it_has_content() {
    let harness = Harness::new(
        vec![text_script("first"), text_script("second")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let harness = harness.attach_memory(Arc::clone(&memory));

    harness.prompt("hi").await;
    let first = harness.model.requests()[0].system[1].clone();
    assert!(!first.contains("<memory"), "got {first:?}");
    assert!(!harness.model.requests()[0].system[0].contains("`<memory>`"));

    seed_memory(
        &memory,
        "The quiet-river lane runs the workspace build before tests.",
        "project",
        Some("prj_runtime_test"),
        4,
    )
    .await;
    harness
        .prompt("hello memory: which lane runs the quiet-river build?")
        .await;

    let requests = harness.model.requests();
    let second = &requests[1];
    assert!(second.system[1].contains("<memory scope=\"project\">"));
    assert!(second.system[1].contains("quiet-river"));
    assert!(second.system[0].contains("`<memory>`"));
}

#[tokio::test]
async fn memory_from_another_project_is_not_injected() {
    let harness = Harness::new(vec![text_script("ok")], Arc::new(AllowAll), json!({}), None).await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "Cross-project secret token that must stay isolated.",
        "project",
        Some("prj_other"),
        5,
    )
    .await;
    let harness = harness.attach_memory(memory);

    harness.prompt("cross-project secret token").await;

    let system = &harness.model.requests()[0].system[1];
    assert!(!system.contains("<memory"), "got {system:?}");
    assert!(!system.contains("Cross-project secret"));
}

#[tokio::test]
async fn skill_index_filters_and_escapes() {
    let harness = Harness::new_with_setup(
        vec![text_script("ok")],
        Arc::new(AllowAll),
        json!({"permission": {"skill": {"gamma": "deny"}}}),
        None,
        |work| {
            write_skill(
                work,
                "alpha",
                "Rust & <Build>",
                Some("Use & verify <carefully>"),
                None,
            );
            write_skill(
                work,
                "beta",
                "Beta Skill",
                Some("Manually invoked only"),
                Some(false),
            );
            write_skill(work, "gamma", "Gamma Skill", Some("Denied by config"), None);
            write_skill(work, "delta", "Delta Skill", None, None);
        },
    )
    .await;

    harness.prompt("hi").await;

    let system = &harness.model.requests()[0].system[1];
    assert!(system.contains("<available_skills>"), "got {system:?}");
    assert!(system.contains("id=\"alpha\""));
    assert!(system.contains("Rust &amp; &lt;Build&gt;"));
    assert!(system.contains("Use &amp; verify &lt;carefully&gt;"));
    assert!(!system.contains("id=\"beta\""));
    assert!(!system.contains("id=\"gamma\""));
    assert!(!system.contains("id=\"delta\""));
    assert!(!system.contains("Rust & <Build>"));
}

#[tokio::test]
async fn agents_file_change_publishes_a_delta_before_the_next_call() {
    let harness = Harness::new_with_setup(
        vec![text_script("first"), text_script("second")],
        Arc::new(AllowAll),
        json!({}),
        None,
        |work| {
            std::fs::write(work.join("AGENTS.md"), "First instruction marker.\n")
                .expect("agents file");
        },
    )
    .await;

    harness.prompt("first").await;
    assert!(harness.model.requests()[0].system[1].contains("First instruction marker."));
    assert_eq!(harness.sink.count("session.instructions.updated"), 0);

    std::fs::write(
        harness.work.join("AGENTS.md"),
        "Second instruction marker.\n",
    )
    .expect("agents update");
    harness.prompt("second").await;

    let kinds = harness.sink.kinds();
    let updated = kinds
        .iter()
        .position(|kind| kind == "session.instructions.updated")
        .expect("instructions delta");
    let last_step = kinds
        .iter()
        .rposition(|kind| kind == "session.step.started")
        .expect("step started");
    assert!(updated < last_step, "delta must precede the next request");

    let data = harness
        .sink
        .data("session.instructions.updated")
        .pop()
        .expect("delta data");
    let delta = data["delta"].as_object().expect("delta map");
    assert!(
        delta.keys().any(|key| key.starts_with("discovery:")),
        "changed source is reported: {delta:?}"
    );
    for (key, value) in delta {
        let value = value.as_str().expect("delta values are strings");
        assert!(
            value == "removed"
                || (value.len() == 16 && value.chars().all(|c| c.is_ascii_hexdigit())),
            "delta[{key}] is a hash or \"removed\", got {value:?}"
        );
    }
    assert!(
        data["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Second instruction marker.")
    );
    let envelope = harness
        .sink
        .events
        .lock()
        .expect("events")
        .iter()
        .find(|event| event.kind == "session.instructions.updated")
        .cloned()
        .expect("instruction envelope");
    let durable = envelope.durable.expect("instruction deltas are durable");
    assert!(durable.seq > 0, "durable events carry a sequence number");
    assert_eq!(durable.version, 2);
    let logged = harness
        .store
        .events_after(&harness.session, 0, 1_000)
        .await
        .expect("event log");
    let stored = logged
        .iter()
        .find(|event| event.kind == "session.instructions.updated")
        .expect("instructions delta persisted before publish");
    assert_eq!(stored.version, 2);
    assert_eq!(stored.seq, durable.seq);

    let requests = harness.model.requests();
    let second = &requests[1].system[1];
    assert!(second.contains("Second instruction marker."));
    assert!(!second.contains("First instruction marker."));
}

#[tokio::test]
async fn memory_write_tool_persists_through_the_merged_registry() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "memory_write",
                "call_write",
                json!({
                    "text": "The build lane is quiet-river.",
                    "kind": "fact",
                    "expectation": expectation()
                }),
            ),
            text_script("stored"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let harness = harness.attach_memory(memory);

    harness.prompt("remember the build lane").await;

    let request = &harness.model.requests()[0];
    assert!(request.tools.iter().any(|tool| tool.name == "memory_write"));
    let rows = all_memories(&harness.store).await;
    assert_eq!(rows.len(), 1, "got {rows:?}");
    assert_eq!(rows[0]["source"], "agent");
    assert_eq!(rows[0]["scope"], "project");
    assert_eq!(rows[0]["scopeID"], "prj_runtime_test");
    assert_eq!(rows[0]["text"], "The build lane is quiet-river.");
}

#[tokio::test]
async fn memory_search_tool_returns_a_compact_block() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "memory_search",
                "call_search",
                json!({"query": "quiet river lane", "expectation": expectation()}),
            ),
            text_script("found it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "The quiet-river lane serializes cargo builds.",
        "project",
        Some("prj_runtime_test"),
        4,
    )
    .await;
    let harness = harness.attach_memory(memory);

    harness
        .prompt("search memory for the quiet river lane")
        .await;

    let result = request_tool_result(&harness.model.requests()[1], 0);
    assert!(
        result.contains("<memory scope=\"project\">"),
        "got {result:?}"
    );
    assert!(result.contains("quiet-river"));
    assert!(result.contains("mem_"));
}

#[tokio::test]
async fn turn_end_touches_injected_memories_once() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    seed_memory(
        &memory,
        "Pinned memory that is always injected.",
        "project",
        Some("prj_runtime_test"),
        5,
    )
    .await;
    let harness = harness.attach_memory(memory);

    harness.prompt("anything").await;

    let rows = all_memories(&harness.store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["uses"], 1);
    assert!(rows[0]["lastUsed"].is_i64());
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn scribe_persists_candidates_in_the_session_scope() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new(
        r#"[{"text":"The runtime merges memory tools into every session registry.","kind":"fact","tags":["runtime"],"importance":3,"confidence":0.9}]"#,
    );
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness
        .prompt("read hello.txt and remember the runtime detail")
        .await;

    let rows = wait_for_memories(&harness.store, "scribe").await;
    assert_eq!(rows.len(), 1, "got {rows:?}");
    assert_eq!(rows[0]["scope"], "session");
    assert_eq!(rows[0]["scopeID"], harness.session);
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn auto_scribe_true_runs_the_turn_end_scribe() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({"memory": {"auto_scribe": true}}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new("[]");
    let prompts = Arc::clone(&scribe);
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness.prompt("read hello.txt").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while prompts.prompts.lock().expect("prompts").is_empty() {
        assert!(Instant::now() < deadline, "scribe never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn auto_scribe_false_skips_the_turn_end_scribe() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({"memory": {"auto_scribe": false}}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new(
        r#"[{"text":"The runtime merges memory tools into every session registry.","kind":"fact","tags":["runtime"],"importance":3,"confidence":0.9}]"#,
    );
    let prompts = Arc::clone(&scribe);
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness.prompt("read hello.txt").await;

    // The turn is already idle; give the spawned pass a bounded window so a
    // regression that ignores the flag is caught rather than timed out.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        prompts.prompts.lock().expect("prompts").is_empty(),
        "memory.auto_scribe=false must not run the scribe"
    );
    assert!(all_memories(&harness.store).await.is_empty());
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn invalid_scribe_json_does_not_fail_the_turn() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "read",
                "call_read",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("the file says hello"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let memory = Arc::new(MemoryService::new(Arc::clone(&harness.store)));
    let scribe = FakeScribe::new("this is not a json array");
    let prompts = Arc::clone(&scribe);
    let harness = harness.attach_memory(memory).attach_scribe(scribe);

    harness.prompt("read hello.txt").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while prompts.prompts.lock().expect("prompts").is_empty() {
        assert!(Instant::now() < deadline, "scribe never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(all_memories(&harness.store).await.is_empty());
    assert!(harness.sink.has("session.execution.succeeded"));
}

#[tokio::test]
async fn runtime_without_memory_keeps_the_old_shape() {
    let harness = Harness::new(
        vec![text_script("plain")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let request = &harness.model.requests()[0];
    assert!(!request.system[1].contains("<memory"));
    assert!(
        !request
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("memory_"))
    );
    assert_eq!(harness.sink.count("session.instructions.updated"), 0);
    assert!(harness.sink.has("session.execution.succeeded"));
}

async fn session_messages(store: &Store, session: &str) -> Vec<Value> {
    let mut cursor = None;
    let mut out = Vec::new();
    loop {
        let page = store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 200,
                order: Order::Asc,
                cursor,
                kind: None,
            })
            .await
            .expect("messages");
        out.extend(page.items);
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

async fn create_child(harness: &Harness, parent: &str, id: &str, agent: &str) -> String {
    let node = harness
        .store
        .session_node(parent)
        .await
        .expect("parent node")
        .expect("parent exists");
    let parent_info = harness
        .store
        .get_session(parent)
        .await
        .expect("parent session")
        .expect("parent exists");
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: id.to_owned(),
                parent_id: Some(parent.to_owned()),
                project_id: parent_info["projectID"]
                    .as_str()
                    .unwrap_or("prj_runtime_test")
                    .to_owned(),
                directory: path(&harness.work),
                title: Some(format!("child {id}")),
                agent: Some(agent.to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: node.root_id,
                depth: node.depth + 1,
                role: "subagent".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("child created");
    id.to_owned()
}

async fn create_unrelated_root(harness: &Harness, id: &str) {
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: id.to_owned(),
                parent_id: None,
                project_id: "prj_unrelated".to_owned(),
                directory: path(&harness.work),
                title: Some("unrelated".to_owned()),
                agent: Some("build".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: id.to_owned(),
                depth: 0,
                role: "primary".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("unrelated root");
}

fn subagent_tool(harness: &Harness, name: &str) -> Arc<dyn Tool> {
    SubagentTools::new(harness.runtime.clone())
        .registry()
        .get(name)
        .cloned()
        .expect("tool registered")
}

fn tool_context(harness: &Harness, session: &str) -> ToolContext {
    ToolContext {
        session_id: session.to_owned(),
        directory: harness.work.clone(),
        cwd: harness.work.clone(),
        cancel: None,
    }
}

async fn child_sessions(store: &Store, parent: &str) -> Vec<String> {
    store
        .list_sessions(SessionQuery {
            parent_id: Some(Some(parent.to_owned())),
            ..SessionQuery::default()
        })
        .await
        .expect("children")
        .items
        .into_iter()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn subagent_foreground_returns_only_the_child_final_text() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "subagent",
                "call_1",
                json!({
                    "agent": "general",
                    "description": "greet",
                    "prompt": "say hi",
                    "background": false,
                }),
            ),
            text_script("child final"),
            text_script("parent done"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1, "one child was created");
    let child = &children[0];
    let messages = session_messages(&harness.store, child).await;
    let visible: Vec<&Value> = messages
        .iter()
        .filter(|message| message["type"] != "idle")
        .collect();
    assert_eq!(visible.len(), 2, "user prompt plus assistant answer");
    assert_eq!(visible[0]["type"], "user");
    assert_eq!(visible[0]["text"], "say hi");
    assert_eq!(visible[1]["type"], "assistant");
    assert_eq!(visible[1]["content"][0]["text"], "child final");

    let requests = harness.model.requests();
    let result = request_tool_result(requests.last().expect("parent request"), 0);
    assert_eq!(
        result,
        format!("<subagent sessionID=\"{child}\" state=\"completed\">child final</subagent>")
    );
    assert!(
        !result.contains("say hi"),
        "the child prompt is not replayed"
    );
    assert!(
        !result.contains("tool"),
        "the child transcript stays private"
    );
}

#[tokio::test]
async fn subagent_foreground_waits_for_a_delayed_child_response() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "subagent",
                "call_delayed",
                json!({
                    "agent": "general",
                    "description": "delayed",
                    "prompt": "think slowly",
                    "background": false,
                }),
            ),
            delayed_text_script("delayed child text", Duration::from_millis(150)),
            text_script("parent done"),
        ],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 5}}),
        None,
    )
    .await;
    harness.prompt("hi").await;

    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1);
    let requests = harness.model.requests();
    let result = request_tool_result(requests.last().expect("parent request"), 0);
    assert!(
        result.contains(">delayed child text</subagent>"),
        "empty or early completion: {result}"
    );
    assert!(result.contains("state=\"completed\""));
}

#[tokio::test]
async fn subagent_background_returns_running_and_the_watcher_reports() {
    let harness = Harness::new(
        vec![text_script("child final")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "background child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let output = result.output.expect("output");
    let status: Value = serde_json::from_str(&output).expect("json output");
    assert_eq!(status["status"], "running");
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert!(
                message["text"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("child final")
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watcher never reported the child"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        session_messages(&harness.store, &child)
            .await
            .iter()
            .filter(|message| message["type"] == "assistant")
            .count(),
        1
    );

    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(5))
        .await
        .expect("parent settles after the synthetic message");
    let delivered: Vec<Value> = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .collect();
    assert_eq!(delivered.len(), 1, "completion is delivered exactly once");
    assert_eq!(delivered[0]["metadata"]["state"], "completed");
    assert!(
        delivered[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("state=\"completed\"")
    );
}

#[tokio::test]
async fn subagent_reports_a_deleted_child_as_failed() {
    let harness = Harness::new(
        vec![Script::HangAfter(Vec::new())],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 60}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let context = tool_context(&harness, &harness.session);
    let handle = tokio::spawn(async move {
        tool.execute(
            &context,
            json!({
                "agent": "general",
                "description": "doomed child",
                "prompt": "work",
                "background": false,
            }),
        )
        .await
    });

    let child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for id in child_sessions(&harness.store, &harness.session).await {
                if !session_messages(&harness.store, &id).await.is_empty() {
                    return id;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child prompt was promoted");
    harness
        .store
        .delete_session(&child)
        .await
        .expect("child deleted");

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("wait_child notices the deleted session")
        .expect("tool task")
        .expect("tool result");
    let output = result.output.expect("output");
    assert!(output.contains("state=\"failed\""), "{output}");
    assert!(output.contains("session deleted"), "{output}");
    assert_eq!(result.metadata.expect("metadata")["status"], "failed");
}

#[tokio::test]
async fn subagent_background_watcher_reports_a_timeout() {
    let harness = Harness::new(
        vec![delayed_text_script("late child", Duration::from_secs(10))],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": true, "timeout_seconds": 1}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "slow child",
                "prompt": "work",
            }),
        )
        .await
        .expect("background spawn");
    let status: Value =
        serde_json::from_str(result.output.as_deref().expect("output")).expect("json output");
    assert_eq!(status["status"], "running");
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    let message = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(message) = session_messages(&harness.store, &harness.session)
                .await
                .into_iter()
                .find(|message| {
                    message["type"] == "synthetic"
                        && message["metadata"]["source"] == "subagent"
                        && message["metadata"]["childID"] == child.as_str()
                })
            {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("bounded watcher reported the timeout");
    let text = message["text"].as_str().unwrap_or_default();
    assert!(text.contains("state=\"failed\""), "{text}");
    assert!(text.contains("reason=\"timeout after 1s\""), "{text}");
    assert_eq!(message["metadata"]["state"], "failed");
    assert_eq!(message["metadata"]["reason"], "timeout after 1s");

    // The timeout cancels the child instead of leaving a zombie behind.
    harness
        .runtime
        .wait_idle(&child, Duration::from_secs(5))
        .await
        .expect("timed-out child settles");
    assert!(
        !harness.runtime.active().contains(&child),
        "timed-out child is still active"
    );
    let child_messages = session_messages(&harness.store, &child).await;
    let idle = child_messages
        .iter()
        .find(|message| message["type"] == "idle")
        .expect("child idle row");
    assert_eq!(idle["outcome"], "interrupted");

    // Exactly one synthetic describes this timeout.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let delivered = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .count();
    assert_eq!(delivered, 1, "timeout must be delivered exactly once");
}

#[tokio::test]
async fn subagent_foreground_timeout_cancels_the_child_once() {
    let harness = Harness::new(
        vec![delayed_text_script("late child", Duration::from_secs(10))],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false, "timeout_seconds": 1}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let started = Instant::now();
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "slow foreground child",
                "prompt": "work",
            }),
        )
        .await
        .expect("foreground spawn");
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(900), "{elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "foreground wait must bound to one timeout: {elapsed:?}"
    );
    let status: Value =
        serde_json::from_str(result.output.as_deref().expect("output")).expect("json output");
    assert_eq!(status["status"], "running");
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    // The failed synthetic must arrive within one short settle window, not a
    // second full timeout.
    let message = tokio::time::timeout(Duration::from_millis(2_500), async {
        loop {
            if let Some(message) = session_messages(&harness.store, &harness.session)
                .await
                .into_iter()
                .find(|message| {
                    message["type"] == "synthetic"
                        && message["metadata"]["source"] == "subagent"
                        && message["metadata"]["childID"] == child.as_str()
                })
            {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("foreground timeout reported within the settle window");
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
        "timeout report took {:?}",
        started.elapsed()
    );
    let text = message["text"].as_str().unwrap_or_default();
    assert!(text.contains("state=\"failed\""), "{text}");
    assert!(text.contains("reason=\"timeout after 1s\""), "{text}");
    assert_eq!(message["metadata"]["state"], "failed");
    assert_eq!(message["metadata"]["reason"], "timeout after 1s");

    harness
        .runtime
        .wait_idle(&child, Duration::from_secs(5))
        .await
        .expect("timed-out child settles");
    assert!(
        !harness.runtime.active().contains(&child),
        "timed-out child is still active"
    );
    let child_messages = session_messages(&harness.store, &child).await;
    let idle = child_messages
        .iter()
        .find(|message| message["type"] == "idle")
        .expect("child idle row");
    assert_eq!(idle["outcome"], "interrupted");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let delivered = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .count();
    assert_eq!(delivered, 1, "timeout must be delivered exactly once");
}

#[tokio::test]
async fn subagent_timeout_drops_a_pending_permission_ask() {
    let handler = Arc::new(PendingReadAsk::default());
    let harness = Harness::new(
        vec![tool_script(
            "call_pending_read",
            json!({"path": "hello.txt", "expectation": expectation()}),
        )],
        handler.clone(),
        json!({
            "permission": {"read": {"*": "ask"}},
            "subagents": {"background_default": false, "timeout_seconds": 1},
        }),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let started = Instant::now();
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "asks for permission",
                "prompt": "read the file",
            }),
        )
        .await
        .expect("foreground spawn");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "foreground wait must bound to one timeout: {:?}",
        started.elapsed()
    );
    let status: Value =
        serde_json::from_str(result.output.as_deref().expect("output")).expect("json output");
    assert_eq!(status["status"], "running");
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    let message = tokio::time::timeout(Duration::from_millis(2_500), async {
        loop {
            if let Some(message) = session_messages(&harness.store, &harness.session)
                .await
                .into_iter()
                .find(|message| {
                    message["type"] == "synthetic"
                        && message["metadata"]["source"] == "subagent"
                        && message["metadata"]["childID"] == child.as_str()
                })
            {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout reported");
    let text = message["text"].as_str().unwrap_or_default();
    assert!(text.contains("reason=\"timeout after 1s\""), "{text}");

    harness
        .runtime
        .wait_idle(&child, Duration::from_secs(5))
        .await
        .expect("timed-out child settles");
    assert!(
        !harness.runtime.active().contains(&child),
        "timed-out child is still active"
    );
    // The permission guard was dropped when the child was cancelled.
    assert_eq!(handler.pending(), 0, "the permission ask is still pending");
    assert_eq!(handler.dropped(), 1, "the ask was raised and dropped once");
    let child_messages = session_messages(&harness.store, &child).await;
    let idle = child_messages
        .iter()
        .find(|message| message["type"] == "idle")
        .expect("child idle row");
    assert_eq!(idle["outcome"], "interrupted");

    let delivered = session_messages(&harness.store, &harness.session)
        .await
        .into_iter()
        .filter(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        })
        .count();
    assert_eq!(delivered, 1, "timeout must be delivered exactly once");
}

#[tokio::test]
async fn subagent_rejects_depth_and_child_limits_before_creation() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {
            "max_depth": 5,
            "max_children_per_session": 0,
            "background_default": false,
        }}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let ctx = tool_context(&harness, &harness.session);

    let depth = tool
        .execute(
            &ctx,
            json!({
                "agent": "general",
                "description": "too deep",
                "prompt": "work",
                "spawn": {"max_depth": 0},
            }),
        )
        .await
        .expect_err("depth limit");
    assert!(depth.message.contains("depth"), "{}", depth.message);
    let children = tool
        .execute(
            &ctx,
            json!({"agent": "general", "description": "too many", "prompt": "work"}),
        )
        .await
        .expect_err("child limit");
    assert!(
        children.message.contains("children"),
        "{}",
        children.message
    );
    assert!(
        child_sessions(&harness.store, &harness.session)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn talk_allows_siblings_parent_and_child_but_rejects_strangers() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"siblings": {"enabled": true}}}),
        None,
    )
    .await;
    let a = create_child(&harness, &harness.session, "ses_child_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_child_b", "general").await;
    create_unrelated_root(&harness, "ses_other_root").await;
    let stranger = create_child(&harness, "ses_other_root", "ses_stranger", "general").await;
    let tool = subagent_tool(&harness, "talk");

    tool.execute(
        &tool_context(&harness, &a),
        json!({"to": b, "kind": "note", "text": "hello sibling"}),
    )
    .await
    .expect("siblings may talk");
    let inbox = harness.store.list_inbox(&b).await.expect("inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0]["type"], "peer");
    assert_eq!(inbox[0]["payload"]["from"], a.as_str());
    assert_eq!(inbox[0]["payload"]["metadata"]["source"], "peer");

    tool.execute(
        &tool_context(&harness, &a),
        json!({"to": harness.session, "kind": "answer", "text": "report"}),
    )
    .await
    .expect("children may answer the parent");
    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({"to": b, "kind": "question", "text": "status?"}),
    )
    .await
    .expect("parents may talk to children");

    let denied = tool
        .execute(
            &tool_context(&harness, &a),
            json!({"to": stranger, "kind": "note", "text": "hi"}),
        )
        .await
        .expect_err("strangers are denied");
    assert!(
        denied.message.contains("not siblings"),
        "{}",
        denied.message
    );
}

#[tokio::test]
async fn roster_returns_ids_agents_statuses_and_titles_only() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let a = create_child(&harness, &harness.session, "ses_roster_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_roster_b", "general").await;
    let tool = subagent_tool(&harness, "roster");
    let result = tool
        .execute(&tool_context(&harness, &a), json!({}))
        .await
        .expect("roster");
    let body: Value = serde_json::from_str(&result.output.expect("output")).expect("json");
    assert_eq!(body["parent"]["id"], harness.session.as_str());
    assert_eq!(body["siblings"].as_array().expect("siblings").len(), 1);
    assert_eq!(body["siblings"][0]["id"], b.as_str());
    assert!(body["children"].as_array().expect("children").is_empty());
    for key in ["id", "agent", "status", "title"] {
        assert!(body["parent"].get(key).is_some(), "missing {key}");
    }
    for field in ["content", "messages", "transcript"] {
        assert!(body["parent"].get(field).is_none(), "leaked {field}");
    }
}

#[tokio::test]
async fn session_write_follows_descendant_ancestry_only() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let a = create_child(&harness, &harness.session, "ses_write_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_write_b", "general").await;
    let tool = subagent_tool(&harness, "session_write");

    let sibling = tool
        .execute(
            &tool_context(&harness, &a),
            json!({"session_id": b, "text": "steer"}),
        )
        .await
        .expect_err("siblings are not writable");
    assert!(
        sibling.message.contains("descendant"),
        "{}",
        sibling.message
    );
    let upward = tool
        .execute(
            &tool_context(&harness, &b),
            json!({"session_id": harness.session, "text": "steer"}),
        )
        .await
        .expect_err("parents are not writable");
    assert!(upward.message.contains("descendant"), "{}", upward.message);

    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({"session_id": b, "text": "from parent", "delivery": "queue"}),
    )
    .await
    .expect("parents write descendants");
    let inbox = harness.store.list_inbox(&b).await.expect("inbox");
    assert!(
        inbox.iter().any(|item| {
            item["type"] == "synthetic"
                && item["payload"]["metadata"]["source"] == "peer"
                && item["payload"]["metadata"]["from"] == harness.session.as_str()
        }),
        "{inbox:?}"
    );
    tool.execute(
        &tool_context(&harness, &b),
        json!({"session_id": b, "text": "self note"}),
    )
    .await
    .expect("self notes are allowed");
}

#[tokio::test]
async fn digest_summarizes_with_a_scribe_caches_and_requires_one() {
    let scribe = FakeScribe::new("focused summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let tool = subagent_tool(&harness, "digest");
    // A tiny budget truncates the raw answer (1 token == 4 chars)...
    let first = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": harness.session, "question": "What happened?", "max_tokens": 1}),
        )
        .await
        .expect("digest");
    assert_eq!(first.output.as_deref(), Some("focu"));
    assert_eq!(first.metadata.expect("metadata")["cached"], false);
    // ...and a cache hit must still honor the caller's own budget instead of
    // replaying the first caller's truncation.
    let second = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": harness.session, "question": "What happened?", "max_tokens": 100}),
        )
        .await
        .expect("cached digest");
    assert_eq!(second.output.as_deref(), Some("focused summary"));
    assert_eq!(second.metadata.expect("metadata")["cached"], true);
    assert_eq!(scribe.prompts.lock().expect("prompts").len(), 1);
    assert!(scribe.prompts.lock().expect("prompts")[0].contains("What happened?"));

    let unsupported = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let error = subagent_tool(&unsupported, "digest")
        .execute(&tool_context(&unsupported, &unsupported.session), json!({}))
        .await
        .expect_err("digest without a scribe");
    assert!(
        error.message.contains("DigestUnavailableError"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn digest_includes_the_complete_final_answer() {
    let scribe = FakeScribe::new("summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let child = create_child(&harness, &harness.session, "ses_digest_final", "general").await;
    for index in 0..60 {
        append_runtime_message(
            &harness,
            &child,
            &format!("msg_digest_ctx_{index}"),
            "user",
            json!({"text": format!("earlier message {index}")}),
        )
        .await;
    }
    append_runtime_message(
        &harness,
        &child,
        "msg_digest_tool",
        "assistant",
        json!({"content": [
            {"type": "text", "text": "checking the build"},
            {
                "type": "tool",
                "id": "call_digest",
                "name": "read",
                "state": {"status": "completed", "input": {}, "content": [], "metadata": {}},
                "time": {"created": 1}
            }
        ]}),
    )
    .await;
    let final_text = format!("BEGIN-REPORT {} END-REPORT", "z".repeat(3_976));
    assert_eq!(final_text.chars().count(), 4_000);
    append_runtime_message(
        &harness,
        &child,
        "msg_digest_final",
        "assistant",
        json!({"content": [{"type": "text", "text": final_text}]}),
    )
    .await;

    subagent_tool(&harness, "digest")
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child, "question": "What did the child report?"}),
        )
        .await
        .expect("digest");

    let prompt = scribe.prompts.lock().expect("prompts")[0].clone();
    assert!(prompt.contains("FINAL ANSWER (complete):"), "{prompt}");
    assert!(prompt.contains(&final_text), "final answer was truncated");
    assert!(!prompt.contains("final answer truncated"), "{prompt}");
    assert!(prompt.contains("RECENT CONTEXT (excerpted):"), "{prompt}");
    assert!(prompt.contains("tool read: completed"), "{prompt}");
    let answer_at = prompt.find("FINAL ANSWER (complete):").expect("marker");
    let context_at = prompt.find("RECENT CONTEXT (excerpted):").expect("marker");
    assert!(answer_at < context_at, "final answer comes first");
}

#[tokio::test]
async fn digest_truncates_an_oversized_final_answer_at_the_budget() {
    let scribe = FakeScribe::new("summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let child = create_child(&harness, &harness.session, "ses_digest_oversize", "general").await;
    let final_text = format!("{}TAIL-MARKER", "y".repeat(6_500));
    append_runtime_message(
        &harness,
        &child,
        "msg_digest_oversize_final",
        "assistant",
        json!({"content": [{"type": "text", "text": final_text}]}),
    )
    .await;

    subagent_tool(&harness, "digest")
        .execute(
            &tool_context(&harness, &harness.session),
            json!({"session_id": child}),
        )
        .await
        .expect("digest");

    let prompt = scribe.prompts.lock().expect("prompts")[0].clone();
    assert!(prompt.contains(&"y".repeat(6_000)), "budget prefix missing");
    assert!(
        prompt.contains("…[final answer truncated at 6000 chars]"),
        "{prompt}"
    );
    assert!(!prompt.contains("TAIL-MARKER"), "exceeded the budget");
}

async fn append_runtime_message(
    harness: &Harness,
    session: &str,
    id: &str,
    kind: &str,
    data: Value,
) {
    harness
        .store
        .append_message(NewMessage {
            id: id.to_owned(),
            session_id: session.to_owned(),
            kind: kind.to_owned(),
            data,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("message appended");
}

#[tokio::test]
async fn digest_is_limited_to_the_callers_subtree() {
    let scribe = FakeScribe::new("scoped summary");
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None)
        .await
        .attach_scribe(Arc::clone(&scribe) as _);
    let child_a = create_child(&harness, &harness.session, "ses_digest_a", "general").await;
    let child_b = create_child(&harness, &harness.session, "ses_digest_b", "general").await;
    create_unrelated_root(&harness, "ses_digest_other").await;
    let tool = subagent_tool(&harness, "digest");

    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({"session_id": child_a, "question": "What did the child do?"}),
    )
    .await
    .expect("a parent digests its descendant");

    tool.execute(
        &tool_context(&harness, &child_a),
        json!({"session_id": child_a}),
    )
    .await
    .expect("a session digests itself");

    for (caller, target) in [
        (child_a.as_str(), harness.session.as_str()),
        (child_a.as_str(), child_b.as_str()),
        (child_a.as_str(), "ses_digest_other"),
        (harness.session.as_str(), "ses_digest_other"),
    ] {
        let error = tool
            .execute(
                &tool_context(&harness, caller),
                json!({"session_id": target}),
            )
            .await
            .expect_err("outside the caller's subtree");
        assert_eq!(
            error.message,
            format!("DigestScopeError: session {target} is outside your subtree")
        );
    }
}

#[tokio::test]
async fn subagent_model_override_resolves_and_unavailable_models_fail_before_spawn() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {
            "background_default": true,
            "default_model": "test/alpha",
            "agents": {"general": {"model": "test/alpha"}},
        }}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    tool.execute(
        &tool_context(&harness, &harness.session),
        json!({
            "agent": "general",
            "description": "modelled",
            "prompt": "work",
            "model": "test/alpha",
        }),
    )
    .await
    .expect("spawn with an available model");
    let children = child_sessions(&harness.store, &harness.session).await;
    assert_eq!(children.len(), 1);
    let session = harness
        .store
        .get_session(&children[0])
        .await
        .expect("session")
        .expect("exists");
    assert_eq!(session["model"]["providerID"], "test");
    assert_eq!(session["model"]["id"], "alpha");

    let error = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "missing",
                "prompt": "work",
                "model": "test/missing",
            }),
        )
        .await
        .expect_err("unavailable model");
    assert!(error.message.contains("unavailable"), "{}", error.message);
    assert!(error.message.contains("test/missing"), "{}", error.message);
    assert_eq!(
        child_sessions(&harness.store, &harness.session).await.len(),
        1,
        "no session is created for an unavailable model"
    );
}

#[tokio::test]
async fn parent_completion_message_carries_the_subagent_metadata() {
    let harness = Harness::new(
        vec![text_script("finished work")],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let tool = subagent_tool(&harness, "subagent");
    let result = tool
        .execute(
            &tool_context(&harness, &harness.session),
            json!({
                "agent": "general",
                "description": "labelled child",
                "prompt": "work",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let child = serde_json::from_str::<Value>(result.output.as_deref().expect("output"))
        .expect("json")["sessionID"]
        .as_str()
        .expect("child")
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let messages = session_messages(&harness.store, &harness.session).await;
        if let Some(message) = messages.iter().find(|message| {
            message["type"] == "synthetic"
                && message["metadata"]["source"] == "subagent"
                && message["metadata"]["childID"] == child.as_str()
        }) {
            assert_eq!(message["metadata"]["agent"], "general");
            assert_eq!(message["metadata"]["state"], "completed");
            let text = message["text"].as_str().expect("envelope");
            assert!(
                text.starts_with(&format!(
                    "<subagent sessionID=\"{child}\" state=\"completed\" description=\"labelled child\">"
                )),
                "{text}"
            );
            assert!(text.contains("finished work"));
            break;
        }
        assert!(Instant::now() < deadline, "no completion message");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn subagent_schemas_match_the_documented_keys() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let registry = SubagentTools::new(harness.runtime.clone()).registry();
    let keys = |name: &str| -> Vec<String> {
        let mut keys: Vec<String> = registry.get(name).expect("tool").schema()["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        keys("subagent"),
        [
            "agent",
            "background",
            "description",
            "lane",
            "model",
            "prompt",
            "role",
            "sessionID",
            "spawn"
        ]
    );
    assert_eq!(keys("talk"), ["kind", "request_reply", "text", "to"]);
    assert!(keys("roster").is_empty());
    assert_eq!(
        keys("session_write"),
        ["delivery", "interrupt", "session_id", "text"]
    );
    assert_eq!(
        keys("digest"),
        ["max_tokens", "question", "session_id", "tool_call_id"]
    );
    let subagent = registry.get("subagent").expect("subagent").schema();
    assert_eq!(subagent["required"], json!(["description", "prompt"]));
    let agent = subagent["properties"]["agent"]
        .as_object()
        .expect("agent schema");
    let agent_description = agent["description"].as_str().expect("agent description");
    assert!(
        agent_description.contains("general"),
        "spawnable agents are named: {agent_description}"
    );
    assert!(
        subagent["properties"]["description"]["type"] == "string"
            && subagent["properties"]["prompt"]["type"] == "string"
    );
    assert_eq!(subagent["additionalProperties"], false);
    assert_eq!(
        subagent["properties"]["role"]["enum"],
        json!(["worker", "coordinator"])
    );
}

#[tokio::test]
async fn subagent_schema_states_the_configured_background_default() {
    let waiting = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": false}}),
        None,
    )
    .await;
    let tool = subagent_tool(&waiting, "subagent");
    let schema = tool.schema();
    let description = schema["properties"]["background"]["description"]
        .as_str()
        .expect("background description");
    assert!(description.contains("Defaults to false"), "{description}");
    assert!(
        description.contains("waits for the child's final text"),
        "{description}"
    );
    assert!(
        description.contains("synthetic parent message"),
        "{description}"
    );

    let immediate = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"background_default": true}}),
        None,
    )
    .await;
    let tool = subagent_tool(&immediate, "subagent");
    let schema = tool.schema();
    let description = schema["properties"]["background"]["description"]
        .as_str()
        .expect("background description");
    assert!(description.contains("Defaults to true"), "{description}");
    assert!(description.contains("\"running\""), "{description}");
    assert!(description.contains("Pass false"), "{description}");
    assert!(
        tool.description().contains("Depending on `background`"),
        "the static description stays neutral: {}",
        tool.description()
    );
}

#[tokio::test]
async fn subagent_tool_is_filtered_out_for_agents_that_cannot_spawn() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let session = "ses_explore_root".to_owned();
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: session.clone(),
                parent_id: None,
                project_id: "prj_runtime_test".to_owned(),
                directory: path(&harness.work),
                title: Some("explore root".to_owned()),
                agent: Some("explore".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: session.clone(),
                depth: 0,
                role: "primary".to_owned(),
                spawn_allowed: true,
            },
        )
        .await
        .expect("explore session");
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "hi", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness.runtime.notify(&session).await.expect("notify");
    harness
        .runtime
        .wait_idle(&session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        !names.contains(&"subagent"),
        "the explore agent denies `subagent`: {names:?}"
    );
    assert!(
        names.contains(&"read"),
        "unrelated tools still reach the explore agent: {names:?}"
    );
}

/// Host provider used to prove that dynamic tools reach the request payload.
struct ProbeTool;

#[async_trait::async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &'static str {
        "provider_probe"
    }

    fn description(&self) -> &'static str {
        "probe tool supplied by a host ToolProvider"
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("probe ok"))
    }
}

struct ProbeProvider;

impl ToolProvider for ProbeProvider {
    fn tools(&self, _session: &oc_types::SessionInfo, _agent: &str) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(ProbeTool)]
    }
}

#[tokio::test]
async fn tool_provider_tools_merge_into_the_registry() {
    let harness = Harness::new(
        vec![text_script("done")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.runtime.add_tool_provider(Arc::new(ProbeProvider));
    harness.prompt("hi").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let probe = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "provider_probe")
        .expect("provider tool reaches the model request");
    let required = probe.input_schema["required"].as_array().expect("schema");
    assert!(required.iter().any(|value| value == "expectation"));
    assert!(
        requests[0].tools.iter().any(|tool| tool.name == "read"),
        "static tools survive the provider merge"
    );
}

#[tokio::test]
async fn goal_check_without_a_goal_still_calls_the_observer() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let notifier = Arc::new(RecordingNotifier::default());
    let (executor, repairs) = RepairChannel::new();
    let model = FixedObserverModel::text(&json!({"action": "none"}));
    let engine = observer_engine(Arc::clone(&model), notifier, executor);
    let runtime = harness.restart().with_observer(ObserverBinding {
        engine,
        goals: goals(&harness),
        repairs,
    });
    runtime.start().await.expect("start");
    assert!(
        harness
            .store
            .get_goal(&harness.session)
            .await
            .expect("goal lookup")
            .is_none(),
        "the root has no goal"
    );

    runtime
        .request_goal_check(&harness.session)
        .await
        .expect("check runs without a goal");
    assert_eq!(
        model.calls(),
        1,
        "check_now is invoked even when the root has no goal"
    );
}

#[tokio::test]
async fn goal_check_without_an_observer_is_a_config_error() {
    let harness = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    assert!(harness.runtime.observer().is_none());
    assert!(harness.runtime.lanes().is_none());
    let error = harness
        .runtime
        .request_goal_check(&harness.session)
        .await
        .expect_err("no observer is configured");
    assert!(matches!(error, oc_runtime::RuntimeError::Config(_)));
}

#[tokio::test]
async fn goal_block_is_injected_into_the_instruction_prefix() {
    let harness = Harness::new(vec![text_script("ok")], Arc::new(AllowAll), json!({}), None).await;
    harness
        .store
        .set_goal(NewGoal {
            id: "gol_prompt".to_owned(),
            root_id: harness.session.clone(),
            objective: "ship the goal prompt".to_owned(),
            criteria: vec!["render the block".to_owned()],
            model: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("goal row");
    harness.prompt("hi").await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    let instructions = requests[0].system.last().expect("instruction part");
    assert!(instructions.contains("<goal id=\"gol_prompt\""));
    assert!(instructions.contains("ship the goal prompt"));
    assert!(instructions.contains("render the block"));
}

#[tokio::test]
async fn talk_publishes_the_peer_message_event_after_the_inbox_row() {
    let harness = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({"subagents": {"siblings": {"enabled": true}}}),
        None,
    )
    .await;
    let a = create_child(&harness, &harness.session, "ses_peer_a", "general").await;
    let b = create_child(&harness, &harness.session, "ses_peer_b", "general").await;
    let tool = subagent_tool(&harness, "talk");
    tool.execute(
        &tool_context(&harness, &a),
        json!({
            "to": b,
            "kind": "question",
            "text": "status?",
            "request_reply": true,
        }),
    )
    .await
    .expect("siblings may talk");

    let events = harness.sink.data("subagent.peer.message");
    assert_eq!(events.len(), 1, "talk publishes exactly one event");
    let data = &events[0];
    assert_eq!(data["sessionID"], b.as_str());
    assert_eq!(data["from"], a.as_str());
    assert_eq!(data["agent"], "general");
    assert_eq!(data["kind"], "question");
    assert_eq!(data["text"], "status?");
    assert_eq!(data["requestReply"], true);

    let inbox = harness.store.list_inbox(&b).await.expect("inbox");
    assert_eq!(inbox.len(), 1);
    assert_eq!(data["messageID"], inbox[0]["id"]);

    let envelope = harness
        .sink
        .events
        .lock()
        .expect("events")
        .iter()
        .find(|event| event.kind == "subagent.peer.message")
        .cloned()
        .expect("peer message envelope");
    assert!(envelope.durable.is_none(), "peer messages are ephemeral");
    let directory = path(&harness.work);
    assert_eq!(
        envelope
            .location
            .as_ref()
            .map(|location| location.directory.as_str()),
        Some(directory.as_str()),
        "location is the target session directory"
    );
}

#[tokio::test]
async fn patch_authorizes_every_target_and_a_denied_one_fails_the_call() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "patch",
                "call_patch",
                json!({
                    "patchText": "*** Begin Patch\n*** Add File: allowed.txt\n+allowed\n*** Add File: blocked.txt\n+blocked\n*** End Patch",
                    "expectation": expectation(),
                }),
            ),
            text_script("patch was denied"),
        ],
        Arc::new(AllowAll),
        json!({"permission": {"edit": {"*blocked.txt": "deny"}}}),
        None,
    )
    .await;
    harness.prompt("apply the patch").await;

    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0]["error"]["type"], "PermissionDeniedError");
    assert_eq!(failures[0]["executed"], false);
    assert!(
        failures[0]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("blocked.txt")
    );
    assert!(
        !harness.work.join("allowed.txt").exists(),
        "authorization runs before any hunk is applied"
    );
    assert!(!harness.work.join("blocked.txt").exists());
}

#[tokio::test]
async fn write_snapshots_the_existing_file_before_overwriting_it() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_snap_write",
                json!({
                    "path": "hello.txt",
                    "content": "replaced\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("wrote it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("replace the greeting").await;

    assert_eq!(
        std::fs::read_to_string(harness.work.join("hello.txt")).expect("written"),
        "replaced\n"
    );
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.path, path(&harness.work.join("hello.txt")));
    assert!(row.existed);
    assert!(!row.skipped);
    assert_eq!(row.content.as_deref(), Some(b"hello world\n".as_slice()));
    assert_eq!(row.tool_call_id, "call_snap_write");

    let messages = harness.messages().await;
    let assistant = assistants(&messages)[0];
    let id = assistant["id"].as_str().expect("assistant id");
    assert_eq!(row.message_id, id);
    let seq = harness
        .store
        .message_seq(&harness.session, id)
        .await
        .expect("seq")
        .expect("message exists");
    assert_eq!(row.seq, i64::try_from(seq).expect("fits"));
}

#[tokio::test]
async fn write_snapshots_a_missing_file_without_content() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_snap_new",
                json!({
                    "path": "fresh.txt",
                    "content": "brand new\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("created it"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("create a file").await;

    assert!(harness.work.join("fresh.txt").exists());
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].path, path(&harness.work.join("fresh.txt")));
    assert!(!rows[0].existed);
    assert!(!rows[0].skipped);
    assert!(rows[0].content.is_none());
}

#[tokio::test]
async fn patch_snapshots_every_target_before_applying() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "patch",
                "call_snap_patch",
                json!({
                    "patchText": "*** Begin Patch\n*** Add File: added.txt\n+new\n*** Update File: hello.txt\n@@\n-hello world\n+goodbye world\n*** End Patch",
                    "expectation": expectation(),
                }),
            ),
            text_script("patched"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("apply the patch").await;

    assert!(
        !harness.sink.has("session.tool.failed"),
        "{:?}",
        harness.sink.data("session.tool.failed")
    );
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert_eq!(rows.len(), 2, "{rows:?}");
    let added = rows
        .iter()
        .find(|row| row.path == path(&harness.work.join("added.txt")))
        .expect("added row");
    let updated = rows
        .iter()
        .find(|row| row.path == path(&harness.work.join("hello.txt")))
        .expect("updated row");
    assert!(!added.existed);
    assert!(added.content.is_none());
    assert!(updated.existed);
    assert_eq!(
        updated.content.as_deref(),
        Some(b"hello world\n".as_slice())
    );
    assert!(rows.iter().all(|row| row.tool_call_id == "call_snap_patch"));
}

#[tokio::test]
async fn writes_outside_the_session_directory_are_not_snapshotted() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_snap_outside",
                json!({
                    "path": "../outside.txt",
                    "content": "external\n",
                    "expectation": expectation(),
                }),
            ),
            text_script("wrote outside"),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    harness.prompt("write outside").await;

    let root = harness.work.parent().expect("temp root");
    assert!(root.join("outside.txt").exists());
    let rows = harness
        .store
        .snapshots_after(&harness.session, -1)
        .await
        .expect("snapshots");
    assert!(rows.is_empty(), "{rows:?}");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn crash_recovery_settles_an_interrupted_background_child_once() {
    let harness = Harness::new(
        vec![text_script("recovered")],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let child = "ses_recovered_child";
    let now = oc_types::now_millis();
    let parent_info = harness
        .store
        .get_session(&harness.session)
        .await
        .expect("parent")
        .expect("parent exists");
    let directory = parent_info["location"]["directory"]
        .as_str()
        .expect("directory")
        .to_owned();
    let project = parent_info["projectID"]
        .as_str()
        .expect("project")
        .to_owned();
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: child.to_owned(),
                parent_id: Some(harness.session.clone()),
                project_id: project,
                directory,
                title: Some("background child".to_owned()),
                agent: Some("general".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: now,
            },
            NewSessionTree {
                root_id: harness.session.clone(),
                depth: 1,
                role: "subagent".to_owned(),
                spawn_allowed: false,
            },
        )
        .await
        .expect("child session");

    // The parent finished its background spawn: the tool part is complete and
    // carries `status:"running"`, which is what a crashed watcher leaves behind.
    harness
        .store
        .append_message(NewMessage {
            id: "msg_parent_bg".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now, "streamed": now, "completed": now},
                "agent": "build",
                "model": {"providerID": "test", "id": "alpha"},
                "finish": "stop",
                "content": [{
                    "type": "tool",
                    "id": "call_bg",
                    "name": "subagent",
                    "executed": true,
                    "state": {
                        "status": "completed",
                        "input": {
                            "description": "background child",
                            "agent": "general",
                            "prompt": "do the work",
                            "background": true
                        },
                        "content": [{
                            "type": "text",
                            "text": format!("{{\"sessionID\":\"{child}\",\"status\":\"running\"}}")
                        }],
                        "metadata": {"sessionID": child, "status": "running"}
                    },
                    "time": {"created": now, "ran": now, "completed": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("parent message");
    // The child died mid-tool.
    harness
        .store
        .append_message(NewMessage {
            id: "msg_child_stale".to_owned(),
            session_id: child.to_owned(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now},
                "agent": "general",
                "model": {"providerID": "test", "id": "alpha"},
                "content": [{
                    "type": "tool",
                    "id": "call_child",
                    "name": "shell",
                    "state": {"status": "running", "input": {"command": "sleep 30"}},
                    "time": {"created": now}
                }]
            }),
            time_created: now,
        })
        .await
        .expect("child message");

    let runtime = harness.restart();
    runtime.start().await.expect("start after crash");
    let deadline = Instant::now() + Duration::from_secs(10);
    let synthetics = loop {
        let messages = harness.messages().await;
        let synthetics: Vec<Value> = messages
            .iter()
            .filter(|message| message["type"] == "synthetic")
            .cloned()
            .collect();
        if !synthetics.is_empty() || Instant::now() > deadline {
            break synthetics;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(synthetics.len(), 1, "{synthetics:?}");
    let synthetic = &synthetics[0];
    assert_eq!(synthetic["metadata"]["childID"], child);
    assert_eq!(synthetic["metadata"]["state"], "failed");
    assert!(
        synthetic["text"]
            .as_str()
            .expect("synthetic text")
            .contains(&format!("sessionID=\"{child}\" state=\"failed\""))
    );
    let child_message = harness
        .store
        .get_message(child, "msg_child_stale")
        .await
        .expect("child message")
        .expect("child row");
    assert_eq!(child_message["finish"], "error");
    assert_eq!(child_message["content"][0]["state"]["status"], "error");

    // A second boot must not duplicate the settlement.
    let runtime = harness.restart();
    runtime.start().await.expect("second start");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let synthetics = harness
        .messages()
        .await
        .iter()
        .filter(|message| message["type"] == "synthetic")
        .count();
    assert_eq!(synthetics, 1);
}
