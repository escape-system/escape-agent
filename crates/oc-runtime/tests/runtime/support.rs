//! Shared fixtures for the runtime integration tests: the scripted model, the
//! recording event sink, permission handlers and the temp-store harness.

use std::collections::VecDeque;
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
use oc_memory::{BoxFuture as MemoryFuture, MemoryError, ScribeModel};
use oc_observer::{
    ObserverConfig, ObserverEngine, ObserverError, ObserverModel, ObserverNotice, ObserverNotifier,
    RepairExecutor, SystemClock,
};
use oc_runtime::{
    EventSink, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler, Runtime,
    RuntimeConfig, StoreGoalStore, SubagentTools,
};
use oc_store::{
    Delivery, MessageQuery, NewInbox, NewSession, NewSessionTree, Order, SessionQuery, Store,
};
use oc_tools::{Tool, ToolContext, ToolRegistry};
use oc_types::{EventEnvelope, MessageId};
use serde_json::{Value, json};
use tempfile::TempDir;

pub(crate) const CATALOG: &str = r#"{
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
    Delayed {
        events: Vec<LlmEvent>,
        delay: Duration,
    },
}

pub(crate) struct ScriptedModel {
    pub(crate) scripts: Mutex<VecDeque<Script>>,
    pub(crate) requests: Mutex<Vec<LlmRequest>>,
    pub(crate) calls: AtomicUsize,
}

impl ScriptedModel {
    pub(crate) fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub(crate) fn requests(&self) -> Vec<LlmRequest> {
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
pub(crate) struct TestSink {
    pub(crate) events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl EventSink for TestSink {
    fn publish(&self, event: EventEnvelope) {
        self.events.lock().expect("events").push(event);
    }
}

impl TestSink {
    pub(crate) fn kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .map(|event| event.kind.clone())
            .collect()
    }

    pub(crate) fn has(&self, kind: &str) -> bool {
        self.events
            .lock()
            .expect("events")
            .iter()
            .any(|event| event.kind == kind)
    }

    pub(crate) fn count(&self, kind: &str) -> usize {
        self.events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    pub(crate) fn data(&self, kind: &str) -> Vec<Value> {
        self.events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.data.clone())
            .collect()
    }
}
pub(crate) struct AllowAll;

impl PermissionHandler for AllowAll {
    fn resolve(&self, _request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        Box::pin(async { PermissionDecision::Allow })
    }
}

pub(crate) struct DenyAll;

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
pub(crate) struct PendingReadAsk {
    pub(crate) pending: Arc<AtomicUsize>,
    pub(crate) dropped: Arc<AtomicUsize>,
}

pub(crate) struct PendingAskGuard {
    pub(crate) pending: Arc<AtomicUsize>,
    pub(crate) dropped: Arc<AtomicUsize>,
}

impl Drop for PendingAskGuard {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::SeqCst);
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl PendingReadAsk {
    pub(crate) fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    pub(crate) fn dropped(&self) -> usize {
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

pub(crate) struct FakeScribe {
    pub(crate) response: String,
    pub(crate) prompts: Mutex<Vec<String>>,
}

impl FakeScribe {
    pub(crate) fn new(response: impl Into<String>) -> Arc<Self> {
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

pub(crate) struct Harness {
    pub(crate) root: TempDir,
    pub(crate) work: PathBuf,
    pub(crate) runtime: Runtime,
    pub(crate) config: RuntimeConfig,
    pub(crate) store: Arc<Store>,
    pub(crate) sink: TestSink,
    pub(crate) model: Arc<ScriptedModel>,
    pub(crate) session: String,
}

impl Harness {
    pub(crate) async fn new(
        scripts: Vec<Script>,
        permissions: Arc<dyn PermissionHandler>,
        config_content: Value,
        session_permissions: Option<Value>,
    ) -> Self {
        Self::new_with_tools(
            scripts,
            permissions,
            config_content,
            session_permissions,
            Arc::new(ToolRegistry::with_defaults()),
        )
        .await
    }

    pub(crate) async fn new_with_setup(
        scripts: Vec<Script>,
        permissions: Arc<dyn PermissionHandler>,
        config_content: Value,
        session_permissions: Option<Value>,
        setup: impl FnOnce(&Path),
    ) -> Self {
        Self::new_with_setup_and_tools(
            scripts,
            permissions,
            config_content,
            session_permissions,
            setup,
            Arc::new(ToolRegistry::with_defaults()),
        )
        .await
    }

    pub(crate) async fn new_with_tools(
        scripts: Vec<Script>,
        permissions: Arc<dyn PermissionHandler>,
        config_content: Value,
        session_permissions: Option<Value>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self::new_with_setup_and_tools(
            scripts,
            permissions,
            config_content,
            session_permissions,
            |_| {},
            tools,
        )
        .await
    }

    async fn new_with_setup_and_tools(
        scripts: Vec<Script>,
        permissions: Arc<dyn PermissionHandler>,
        config_content: Value,
        session_permissions: Option<Value>,
        setup: impl FnOnce(&Path),
        tools: Arc<ToolRegistry>,
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
            tools,
            sink: Arc::new(sink.clone()),
            permissions,
        };
        let runtime = Runtime::new_with_model_client(config.clone(), Arc::clone(&model) as _);
        Self {
            root,
            work,
            runtime,
            config,
            store,
            sink,
            model,
            session,
        }
    }

    pub(crate) async fn enqueue(&self, text: &str) -> String {
        self.enqueue_with(text, Delivery::Steer).await
    }

    pub(crate) async fn enqueue_with(&self, text: &str, delivery: Delivery) -> String {
        let id = MessageId::new().into_inner();
        self.store
            .enqueue(NewInbox {
                id: id.clone(),
                session_id: self.session.clone(),
                kind: "user".to_owned(),
                payload: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
                delivery,
            })
            .await
            .expect("enqueued");
        id
    }

    /// How many `session.inbox.delivered` events this session published for
    /// `inbox_id`.
    pub(crate) fn delivery_count(&self, inbox_id: &str) -> usize {
        self.sink
            .data("session.inbox.delivered")
            .iter()
            .filter(|data| {
                data["sessionID"].as_str() == Some(self.session.as_str())
                    && data["inboxID"].as_str() == Some(inbox_id)
            })
            .count()
    }

    pub(crate) async fn prompt(&self, text: &str) {
        self.enqueue(text).await;
        self.runtime.notify(&self.session).await.expect("notify");
        self.runtime
            .wait_idle(&self.session, Duration::from_secs(10))
            .await
            .expect("idle");
    }

    pub(crate) fn restart(&self) -> Runtime {
        Runtime::new_with_model_client(self.config.clone(), Arc::clone(&self.model) as _)
    }

    pub(crate) fn attach_memory(mut self, memory: Arc<oc_memory::MemoryService>) -> Self {
        self.runtime = self.runtime.clone().with_memory(memory);
        self
    }

    pub(crate) fn attach_scribe(mut self, scribe: Arc<dyn oc_memory::ScribeModel>) -> Self {
        self.runtime = self.runtime.clone().with_scribe(scribe);
        self
    }

    pub(crate) async fn inbox(&self) -> Vec<Value> {
        self.store.list_inbox(&self.session).await.expect("inbox")
    }

    pub(crate) async fn messages(&self) -> Vec<Value> {
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

    pub(crate) async fn wait_for_event(&self, kind: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.sink.has(kind) {
            assert!(Instant::now() < deadline, "timed out waiting for {kind}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
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

pub(crate) fn request_contains(request: &LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .filter_map(oc_llm::ContentPart::as_text)
            .any(|text| text.contains(needle))
    })
}

pub(crate) fn text_script(text: &str) -> Script {
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

pub(crate) fn delayed_text_script(text: &str, delay: Duration) -> Script {
    match text_script(text) {
        Script::Events(events) => Script::Delayed { events, delay },
        _ => unreachable!("text_script always returns events"),
    }
}

pub(crate) fn tool_script(call_id: &str, input: Value) -> Script {
    named_tool_script("read", call_id, input)
}

pub(crate) fn named_tool_script(name: &str, call_id: &str, input: Value) -> Script {
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

pub(crate) fn request_tool_result(request: &LlmRequest, index: usize) -> String {
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

pub(crate) fn expectation() -> Value {
    json!({
        "why": "read the greeting file",
        "expect_seconds": 5,
        "intervene_after_seconds": 7
    })
}

pub(crate) fn assistants(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))
        .collect()
}

pub(crate) async fn create_child(harness: &Harness, parent: &str, id: &str, agent: &str) -> String {
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

pub(crate) async fn create_unrelated_root(harness: &Harness, id: &str) {
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

pub(crate) fn subagent_tool(harness: &Harness, name: &str) -> Arc<dyn Tool> {
    SubagentTools::new(harness.runtime.clone())
        .registry()
        .get(name)
        .cloned()
        .expect("tool registered")
}

pub(crate) fn tool_context(harness: &Harness, session: &str) -> ToolContext {
    ToolContext {
        session_id: session.to_owned(),
        directory: harness.work.clone(),
        cwd: harness.work.clone(),
        cancel: None,
    }
}

pub(crate) async fn child_sessions(store: &Store, parent: &str) -> Vec<String> {
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
pub(crate) enum ObserverReply {
    Text(String),
    Error(String),
}

pub(crate) struct FixedObserverModel {
    pub(crate) reply: ObserverReply,
    pub(crate) calls: AtomicUsize,
}

impl FixedObserverModel {
    pub(crate) fn text(text: &Value) -> Arc<Self> {
        Arc::new(Self {
            reply: ObserverReply::Text(text.to_string()),
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn error(message: &str) -> Arc<Self> {
        Arc::new(Self {
            reply: ObserverReply::Error(message.to_owned()),
            calls: AtomicUsize::new(0),
        })
    }

    pub(crate) fn calls(&self) -> usize {
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
pub(crate) struct RecordingNotifier {
    pub(crate) notices: Mutex<Vec<ObserverNotice>>,
}

impl ObserverNotifier for RecordingNotifier {
    fn notify(&self, event: ObserverNotice) {
        self.notices.lock().expect("notices").push(event);
    }
}

impl RecordingNotifier {
    pub(crate) fn has_unavailable(&self) -> bool {
        self.notices
            .lock()
            .expect("notices")
            .iter()
            .any(|notice| matches!(notice, ObserverNotice::Unavailable { .. }))
    }
}

pub(crate) fn observer_config() -> ObserverConfig {
    ObserverConfig {
        tick_ms: 20,
        min_interval_ms: 0,
        ..ObserverConfig::default()
    }
}

pub(crate) fn observer_engine(
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

pub(crate) fn goals(harness: &Harness) -> Arc<dyn oc_observer::GoalStore> {
    Arc::new(StoreGoalStore::new(Arc::clone(&harness.store)))
}
