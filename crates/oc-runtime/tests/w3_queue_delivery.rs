//! Issue #3 regression: `delivery: "queue"` wakes idle subagents and queues
//! sequentially behind busy turns without injecting mid-turn.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{
    Catalog, LlmClient, LlmError, LlmEvent, LlmRequest, LlmStream, SharedLlmClient, Usage,
};
use oc_runtime::{
    EventSink, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler, Runtime,
    RuntimeConfig, SubagentTools,
};
use oc_store::{Delivery, MessageQuery, NewInbox, NewSession, NewSessionTree, Order, Store};
use oc_tools::{Tool, ToolContext, ToolRegistry};
use oc_types::{EventEnvelope, MessageId};
use serde_json::{Value, json};
use tempfile::TempDir;

const CATALOG: &str = r#"{
  "test": {
    "id": "test", "name": "Test Provider", "env": ["TEST_API_KEY"],
    "models": {
      "alpha": {
        "id": "alpha", "type": "chat", "name": "Alpha",
        "release_date": "2025-01-02",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {"tools": {"supported": true}, "temperature": true},
        "limit": {"context": 100000, "output": 10000},
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
        "api": {"chat-completions": {"base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
      }
    }
  }
}"#;

#[derive(Clone, Default)]
struct Sink {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl EventSink for Sink {
    fn publish(&self, event: EventEnvelope) {
        self.events.lock().expect("events").push(event);
    }
}

impl Sink {
    fn count(&self, kind: &str) -> usize {
        self.events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }
}

struct AllowAll;

impl PermissionHandler for AllowAll {
    fn resolve(&self, _request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        Box::pin(async { PermissionDecision::Allow })
    }
}

/// One answer per provider call. This is enough to observe calls (not content),
/// which is what queue-vs-steer delivery hinges on.
struct ScriptModel {
    texts: Mutex<VecDeque<&'static str>>,
    calls: AtomicUsize,
    hang: bool,
}

impl ScriptModel {
    fn new(texts: Vec<&'static str>) -> Self {
        Self {
            texts: Mutex::new(texts.into()),
            calls: AtomicUsize::new(0),
            hang: false,
        }
    }

    /// Keep the first call pending forever, so a spawned child stays `busy`
    /// while the parent's `roster(wait_seconds)` runs.
    fn hanging() -> Self {
        Self {
            texts: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            hang: true,
        }
    }
}

impl ModelClient for ScriptModel {
    fn stream(&self, _request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self
            .texts
            .lock()
            .expect("scripts")
            .pop_front()
            .unwrap_or("done")
            .to_owned();
        let hang = self.hang;
        Box::pin(async move {
            if hang {
                // A live stream that never finishes: the child session stays
                // `busy` for the whole roster wait, as in the reported bug.
                return Ok(stream::pending::<LlmEvent>().boxed());
            }
            let usage = Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                total_tokens: Some(15),
                ..Usage::default()
            };
            let events = vec![
                LlmEvent::StepStart,
                LlmEvent::TextStart,
                LlmEvent::TextDelta {
                    delta: text.clone(),
                },
                LlmEvent::TextEnd { text },
                LlmEvent::Finish {
                    reason: "stop".to_owned(),
                    usage,
                },
            ];
            Ok(stream::iter(events).boxed())
        })
    }
}

struct Harness {
    _root: TempDir,
    work: std::path::PathBuf,
    runtime: Runtime,
    store: Arc<Store>,
    sink: Sink,
    model: Arc<ScriptModel>,
    session: String,
}

impl Harness {
    async fn new(texts: Vec<&'static str>) -> Self {
        Self::with_model(ScriptModel::new(texts)).await
    }

    /// Build a harness around an already-configured model. The runtime takes an
    /// `Arc<ScriptModel>`, so behavior must be chosen before that Arc exists.
    async fn with_model(model: ScriptModel) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        let config = ConfigService::load(
            LoadOptions::new(&work)
                .with_env("HOME", path(root.path()))
                .with_env("USERPROFILE", path(root.path()))
                .with_env("XDG_CONFIG_HOME", path(&root.path().join("config")))
                .with_env("ProgramData", path(&root.path().join("programdata")))
                .with_env("OPENCODE_CONFIG", path(&root.path().join("missing.json")))
                .with_env("OPENCODE_CONFIG_CONTENT", json!({}).to_string()),
        )
        .expect("config loads");
        let store = Arc::new(
            Store::open(root.path().join("db.sqlite"))
                .await
                .expect("store opens"),
        );
        let session = "ses_w3_root".to_owned();
        store
            .create_session_with_tree(
                NewSession {
                    id: session.clone(),
                    parent_id: None,
                    project_id: "prj_w3".to_owned(),
                    directory: path(&work),
                    title: Some("w3 test".to_owned()),
                    agent: Some("engineer".to_owned()),
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
            .expect("root session");
        let model = Arc::new(model);
        let sink = Sink::default();
        let config = RuntimeConfig {
            store: Arc::clone(&store),
            config: Arc::new(config),
            catalog: Arc::new(oc_llm::CatalogCell::from_arc(Arc::new(
                Catalog::from_snapshot(CATALOG).expect("catalog"),
            ))),
            client: Arc::new(SharedLlmClient::new(LlmClient::new().expect("client"))),
            tools: Arc::new(ToolRegistry::with_defaults()),
            sink: Arc::new(sink.clone()),
            permissions: Arc::new(AllowAll),
        };
        let runtime = Runtime::new_with_model_client(config, Arc::clone(&model) as _);
        Self {
            _root: root,
            work,
            runtime,
            store,
            sink,
            model,
            session,
        }
    }

    async fn enqueue_to(&self, session: &str, text: &str, delivery: Delivery) -> String {
        let id = MessageId::new().into_inner();
        self.store
            .enqueue(NewInbox {
                id: id.clone(),
                session_id: session.to_owned(),
                kind: "user".to_owned(),
                payload: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
                delivery,
            })
            .await
            .expect("enqueue");
        id
    }

    async fn create_child(&self, id: &str) -> String {
        self.store
            .create_session_with_tree(
                NewSession {
                    id: id.to_owned(),
                    parent_id: Some(self.session.clone()),
                    project_id: "prj_w3".to_owned(),
                    directory: path(&self.work),
                    title: Some(id.to_owned()),
                    agent: Some("general".to_owned()),
                    model: Some(json!({"providerID": "test", "id": "alpha"})),
                    metadata: None,
                    permissions: None,
                    time_created: oc_types::now_millis(),
                },
                NewSessionTree {
                    root_id: self.session.clone(),
                    depth: 1,
                    role: "subagent".to_owned(),
                    spawn_allowed: false,
                },
            )
            .await
            .expect("child session");
        id.to_owned()
    }

    async fn messages(&self, session: &str) -> Vec<Value> {
        self.store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 200,
                order: Order::Asc,
                cursor: None,
                kind: None,
            })
            .await
            .expect("messages")
            .items
    }

    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        SubagentTools::new(self.runtime.clone())
            .registry()
            .get(name)
            .cloned()
            .expect("tool registered")
    }

    fn context(&self, session: &str) -> ToolContext {
        ToolContext {
            session_id: session.to_owned(),
            directory: self.work.clone(),
            cwd: self.work.clone(),
            cancel: None,
        }
    }
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// (a) Writing to an idle child with `delivery: "queue"` wakes and delivers
/// the prompt to the child instead of leaving it stranded in SQLite or letting
/// the child watcher time out with "child never started".
#[tokio::test]
async fn queue_row_via_session_write_wakes_idle_child_and_delivers() {
    let harness = Harness::new(vec!["queued reply"]).await;
    let child = harness.create_child("ses_w3_child").await;
    let write = harness.tool("session_write");

    let output = write
        .execute(
            &harness.context(&harness.session),
            json!({
                "session_id": child,
                "text": "QUEUED_NOTE",
                "delivery": "queue",
            }),
        )
        .await
        .expect("session_write queue succeeds");
    assert!(
        output
            .output
            .as_deref()
            .unwrap_or_default()
            .contains("delivery: queue"),
        "expected queue confirmation: {:?}",
        output.output
    );

    harness
        .runtime
        .wait_idle(&child, Duration::from_secs(10))
        .await
        .expect("child finishes queued turn");

    assert_eq!(
        harness.model.calls.load(Ordering::SeqCst),
        1,
        "idle child must wake and run the queued turn"
    );
    assert!(
        harness.sink.count("session.inbox.delivered") >= 1,
        "queue delivery must publish session.inbox.delivered"
    );
    let child_msgs = harness.messages(&child).await;
    let mut child_texts: Vec<String> = Vec::new();
    for m in &child_msgs {
        if let Some(t) = m.get("text").and_then(Value::as_str) {
            child_texts.push(t.to_owned());
        }
        if let Some(parts) = m.get("content").and_then(Value::as_array) {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    child_texts.push(t.to_owned());
                }
            }
        }
    }
    assert!(
        child_texts.iter().any(|t| t == "QUEUED_NOTE"),
        "child must receive QUEUED_NOTE, got: {child_texts:?}"
    );
    assert!(
        child_texts.iter().any(|t| t == "queued reply"),
        "child must complete its assistant reply, got: {child_texts:?}"
    );
    assert_eq!(
        harness.store.list_inbox(&child).await.expect("inbox").len(),
        0,
        "queued inbox row must be drained"
    );
}

/// (c) Queue vs steer FIFO ordering when two rows are present before wake:
/// both rows stay durable until delivered cleanly one turn after another.
#[tokio::test]
async fn queue_then_steer_deliver_fifo_without_stranding() {
    let harness = Harness::new(vec!["after queue", "after steer"]).await;
    let child = harness.create_child("ses_w3_fifo").await;

    let _queued = harness
        .enqueue_to(&child, "QUEUED_NOTE", Delivery::Queue)
        .await;
    let _steer = harness
        .enqueue_to(&child, "STEER_NOW", Delivery::Steer)
        .await;
    harness.runtime.notify(&child).await.expect("notify");
    harness
        .runtime
        .wait_idle(&child, Duration::from_secs(10))
        .await
        .expect("idle");

    assert_eq!(harness.model.calls.load(Ordering::SeqCst), 2);
    let texts: Vec<String> = harness
        .messages(&child)
        .await
        .iter()
        .filter_map(|m| m["text"].as_str().map(str::to_owned))
        .collect();
    let qpos = texts
        .iter()
        .position(|t| t == "QUEUED_NOTE")
        .expect("queued note delivered");
    let spos = texts
        .iter()
        .position(|t| t == "STEER_NOW")
        .expect("steer delivered");
    assert!(qpos < spos, "FIFO order preserved: {texts:?}");
    assert_eq!(
        harness.store.list_inbox(&child).await.expect("inbox").len(),
        0
    );
}

/// (b) `roster(wait_seconds)` wakes on a queue row even when the only busy
/// child stays busy, so a parent never misses a durable note while waiting.
#[tokio::test]
async fn roster_wait_wakes_on_queue_row() {
    // Start a real background child whose provider call never completes, so the
    // session status stays `busy` — the exact state where a queue row used to
    // be invisible to `roster(wait_seconds)`. The hanging model is constructed
    // before the harness wraps it in the Arc shared with the runtime.
    let harness = Harness::with_model(ScriptModel::hanging()).await;
    let spawn = harness.tool("subagent");
    let spawned = spawn
        .execute(
            &harness.context(&harness.session),
            json!({
                "agent": "general",
                "description": "hanging worker",
                "prompt": "never finish",
                "background": true,
            }),
        )
        .await
        .expect("background spawn");
    let status: Value =
        serde_json::from_str(spawned.output.as_deref().expect("output")).expect("json");
    assert_eq!(
        status["status"], "running",
        "child must be busy for this test"
    );
    let child = status["sessionID"].as_str().expect("child id").to_owned();

    let tool = harness.tool("roster");
    let caller = harness.context(&harness.session);
    let queued = harness
        .enqueue_to(&harness.session, "CHILD_QUEUE_NOTE", Delivery::Queue)
        .await;

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(&caller, json!({"wait_seconds": 3})),
    )
    .await
    .expect("roster must return before the wait window elapses")
    .expect("roster succeeds");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "queue row should wake roster immediately, took {elapsed:?}"
    );
    let body: Value =
        serde_json::from_str(result.output.as_deref().expect("output")).expect("json");
    assert_eq!(body["children"][0]["id"], child.as_str());
    assert!(
        harness
            .store
            .list_inbox(&harness.session)
            .await
            .expect("inbox")
            .iter()
            .any(|row| row["id"] == queued.as_str()),
        "the row is still durable after waking roster"
    );
}
