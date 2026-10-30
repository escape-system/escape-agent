//! Offline tests for the observer repair commands that resolve their own
//! defaults: `repost` (last user prompt), `restart_subagent` (replay the child
//! prompt) and `switch_model` (`observer.escalation_model` fallback).

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{Catalog, LlmClient, LlmError, LlmEvent, LlmRequest, LlmStream, SharedLlmClient};
use oc_observer::{
    ObserverConfig, ObserverEngine, ObserverError, ObserverModel, ObserverNotice, ObserverNotifier,
    RepairExecutor, RepairOutcome, SystemClock,
};
use oc_runtime::{
    EventSink, PermissionAsk, PermissionDecision, PermissionHandler, RepairCommand, Runtime,
    RuntimeConfig, StoreGoalStore,
};
use oc_store::{
    Delivery, MessageQuery, NewInbox, NewMessage, NewSession, NewSessionTree, Order, Store,
};
use oc_tools::ToolRegistry;
use oc_types::{EventEnvelope, MessageId};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedSender;

const ROOT: &str = "ses_observer_root";
const OTHER: &str = "ses_observer_other";

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
      },
      "beta": {
        "id": "beta",
        "type": "chat",
        "name": "Beta",
        "release_date": "2025-01-03",
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

fn path(value: &Path) -> String {
    value.to_string_lossy().into_owned()
}

struct EmptyModel;

impl oc_runtime::ModelClient for EmptyModel {
    fn stream(&self, _request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        Box::pin(async { Ok(stream::empty::<LlmEvent>().boxed()) })
    }
}

/// Keeps a turn running until it is interrupted, so guards that depend on
/// `is_idle` can be exercised.
struct HangingModel;

impl oc_runtime::ModelClient for HangingModel {
    fn stream(&self, _request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        Box::pin(async { Ok(stream::pending::<LlmEvent>().boxed()) })
    }
}

struct AllowAll;

impl PermissionHandler for AllowAll {
    fn resolve(&self, _request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        Box::pin(async { PermissionDecision::Allow })
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

struct NoopObserver;

impl ObserverModel for NoopObserver {
    fn judge<'a>(
        &'a self,
        _input: &'a str,
        _cfg: &'a ObserverConfig,
    ) -> oc_observer::BoxFuture<'a, Result<String, ObserverError>> {
        Box::pin(async { Ok("{\"action\":\"none\"}".to_owned()) })
    }
}

struct NoopNotifier;

impl ObserverNotifier for NoopNotifier {
    fn notify(&self, _event: ObserverNotice) {}
}

/// The engine is not driven in these tests; the channel executor is a dummy.
struct NeverExecutor;

impl RepairExecutor for NeverExecutor {
    fn apply<'a>(
        &'a self,
        _verdict: &'a oc_observer::ObserverVerdict,
    ) -> oc_observer::BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        Box::pin(async {
            Err(ObserverError::Executor(
                "repairs are injected directly in this test".to_owned(),
            ))
        })
    }
}

struct Harness {
    _root: TempDir,
    store: Arc<Store>,
    sink: TestSink,
    sender: UnboundedSender<RepairCommand>,
    runtime: Runtime,
}

impl Harness {
    async fn new(config_content: Value) -> Self {
        Self::with_model(config_content, Arc::new(EmptyModel)).await
    }

    async fn with_model(config_content: Value, model: Arc<dyn oc_runtime::ModelClient>) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).expect("work dir");
        std::fs::write(work.join("hello.txt"), "hello world\n").expect("file");

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
        create_root(&store, &path(&work), ROOT).await;

        let catalog = Arc::new(Catalog::from_snapshot(CATALOG).expect("catalog"));
        let client = Arc::new(SharedLlmClient::new(LlmClient::new().expect("client")));
        let sink = TestSink::default();
        let config = RuntimeConfig {
            store: Arc::clone(&store),
            config: Arc::new(config),
            catalog,
            client,
            tools: Arc::new(ToolRegistry::with_defaults()),
            sink: Arc::new(sink.clone()),
            permissions: Arc::new(AllowAll),
        };
        let engine = Arc::new(ObserverEngine::new(
            ObserverConfig {
                tick_ms: 10_000,
                ..ObserverConfig::default()
            },
            Arc::new(SystemClock),
            Arc::new(NoopObserver),
            Arc::new(NeverExecutor),
            Arc::new(NoopNotifier),
        ));
        let (sender, repairs) = tokio::sync::mpsc::unbounded_channel();
        let goals: Arc<dyn oc_observer::GoalStore> =
            Arc::new(StoreGoalStore::new(Arc::clone(&store)));
        let runtime = Runtime::new_with_model_client(config, model).with_observer(
            oc_runtime::ObserverBinding {
                engine,
                goals,
                repairs,
            },
        );
        runtime.start().await.expect("start");
        Self {
            _root: root,
            store,
            sink,
            sender,
            runtime,
        }
    }

    fn send(&self, command: RepairCommand) {
        self.sender.send(command).expect("repair command");
    }
}

async fn create_root(store: &Store, directory: &str, id: &str) {
    store
        .create_session(NewSession {
            id: id.to_owned(),
            parent_id: None,
            project_id: "prj_observer_repair".to_owned(),
            directory: directory.to_owned(),
            title: Some("observer repair test".to_owned()),
            agent: Some("build".to_owned()),
            model: Some(json!({"providerID": "test", "id": "alpha"})),
            metadata: None,
            permissions: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("session created");
}

async fn create_child(harness: &Harness, parent: &str, id: &str, directory: &str) -> String {
    let node = harness
        .store
        .session_node(parent)
        .await
        .expect("parent node")
        .expect("parent exists");
    harness
        .store
        .create_session_with_tree(
            NewSession {
                id: id.to_owned(),
                parent_id: Some(parent.to_owned()),
                project_id: "prj_observer_repair".to_owned(),
                directory: directory.to_owned(),
                title: Some(format!("child {id}")),
                agent: Some("general".to_owned()),
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

async fn append_user(harness: &Harness, session: &str, id: &str, text: &str) {
    harness
        .store
        .append_message(NewMessage {
            id: id.to_owned(),
            session_id: session.to_owned(),
            kind: "user".to_owned(),
            data: json!({"text": text}),
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("user message");
}

async fn enqueue_user(harness: &Harness, session: &str, text: &str) {
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: session.to_owned(),
            kind: "user".to_owned(),
            payload: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("user inbox row");
}

/// Observer-flagged synthetic payload of an inbox row or a promoted message.
fn observer_payload(value: &Value) -> Option<&Value> {
    let payload = value.get("payload").unwrap_or(value);
    let observer = payload
        .get("metadata")
        .and_then(|metadata| metadata.get("observer"))
        .and_then(Value::as_bool);
    (observer == Some(true)).then_some(payload)
}

fn observer_text(value: &Value) -> Option<&str> {
    observer_payload(value)
        .and_then(|payload| payload.get("text"))
        .and_then(Value::as_str)
}

fn observer_target(value: &Value) -> Option<&str> {
    observer_payload(value)
        .and_then(|payload| payload.get("metadata"))
        .and_then(|metadata| metadata.get("target"))
        .and_then(Value::as_str)
}

async fn observer_rows(store: &Store, session: &str) -> Vec<Value> {
    let mut rows = store.list_inbox(session).await.expect("inbox");
    let page = store
        .list_messages(MessageQuery {
            session_id: session.to_owned(),
            limit: 200,
            order: Order::Asc,
            cursor: None,
            kind: Some("synthetic".to_owned()),
        })
        .await
        .expect("messages");
    rows.extend(page.items);
    rows
}

async fn wait_for_observer_text(store: &Store, session: &str, expected: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(found) = observer_rows(store, session)
            .await
            .into_iter()
            .find(|row| observer_text(row) == Some(expected))
        {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for an observer synthetic row with {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_model(store: &Store, session: &str, model: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let row = store
            .get_session(session)
            .await
            .expect("session")
            .expect("session exists");
        if row["model"]["id"].as_str() == Some(model) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {session} to switch to {model}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn repost_without_text_replays_the_latest_user_prompt() {
    let harness = Harness::new(json!({})).await;
    append_user(&harness, ROOT, "msg_repair_first", "first prompt").await;
    append_user(&harness, ROOT, "msg_repair_last", "latest prompt").await;

    harness.send(RepairCommand::Repost {
        session_id: ROOT.to_owned(),
        text: String::new(),
    });

    let row = wait_for_observer_text(&harness.store, ROOT, "latest prompt").await;
    assert_eq!(observer_target(&row), Some(ROOT));
}

#[tokio::test]
async fn repost_does_not_duplicate_pending_input() {
    let harness = Harness::new(json!({})).await;
    append_user(&harness, ROOT, "msg_repair_pending", "latest prompt").await;
    enqueue_user(&harness, ROOT, "already waiting").await;
    create_root(&harness.store, ".", OTHER).await;

    harness.send(RepairCommand::Repost {
        session_id: ROOT.to_owned(),
        text: String::new(),
    });
    // A later explicit switch on another session is the barrier that proves
    // the repost command was processed.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
    });
    wait_for_model(&harness.store, OTHER, "beta").await;

    assert!(
        observer_rows(&harness.store, ROOT)
            .await
            .iter()
            .all(|row| observer_text(row).is_none()),
        "a pending deliverable row must suppress the repost"
    );
}

#[tokio::test]
async fn repost_is_skipped_while_a_turn_is_running() {
    let harness = Harness::with_model(json!({}), Arc::new(HangingModel)).await;
    create_root(&harness.store, ".", OTHER).await;
    append_user(&harness, ROOT, "msg_repair_running", "latest prompt").await;
    enqueue_user(&harness, ROOT, "start").await;
    harness.runtime.notify(ROOT).await.expect("notify");
    wait_until_running(&harness.runtime, ROOT).await;

    harness.send(RepairCommand::Repost {
        session_id: ROOT.to_owned(),
        text: String::new(),
    });
    // The running prompt is no longer in the inbox, so only the idle check can
    // stop a duplicate execution.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
    });
    wait_for_model(&harness.store, OTHER, "beta").await;

    assert!(
        observer_rows(&harness.store, ROOT)
            .await
            .iter()
            .all(|row| observer_text(row).is_none()),
        "a running turn must suppress the repost"
    );

    harness.runtime.interrupt(ROOT).await.expect("interrupt");
}

async fn wait_until_running(runtime: &Runtime, session: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if runtime.active().iter().any(|id| id == session) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {session} to become active"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn switch_model_rejects_a_model_outside_the_catalog() {
    let harness = Harness::new(json!({})).await;
    create_root(&harness.store, ".", OTHER).await;

    harness.send(RepairCommand::SwitchModel {
        session_id: ROOT.to_owned(),
        model: "test/ghost".to_owned(),
    });
    // A valid switch on another session is the barrier that proves the ghost
    // command was processed before we read ROOT.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
    });
    wait_for_model(&harness.store, OTHER, "beta").await;

    let row = harness
        .store
        .get_session(ROOT)
        .await
        .expect("session")
        .expect("root");
    assert_eq!(
        row["model"]["id"], "alpha",
        "a model missing from the catalog must not be persisted"
    );
}

#[tokio::test]
async fn restart_subagent_replays_the_child_prompt_with_the_note() {
    let harness = Harness::new(json!({})).await;
    let child = create_child(&harness, ROOT, "ses_observer_child", ".").await;
    append_user(&harness, &child, "msg_child_prompt", "please fix the build").await;

    harness.send(RepairCommand::RestartSubagent {
        session_id: child.clone(),
        note: "the child stalled".to_owned(),
    });

    let expected = "please fix the build\n\n[observer restart] the child stalled";
    let row = wait_for_observer_text(&harness.store, &child, expected).await;
    assert_eq!(observer_target(&row), Some(ROOT));
}

#[tokio::test]
async fn switch_model_without_a_model_uses_the_escalation_model() {
    let harness = Harness::new(json!({
        "observer": {"escalation_model": "test/beta"}
    }))
    .await;

    harness.send(RepairCommand::SwitchModel {
        session_id: ROOT.to_owned(),
        model: String::new(),
    });

    wait_for_model(&harness.store, ROOT, "beta").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while harness.sink.count("session.model.selected") == 0 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for session.model.selected"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let events = harness.sink.data("session.model.selected");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["sessionID"], ROOT);
    assert_eq!(events[0]["model"]["id"], "beta");
}

#[tokio::test]
async fn switch_model_without_any_configured_model_changes_nothing() {
    let harness = Harness::new(json!({})).await;
    create_root(&harness.store, ".", OTHER).await;

    harness.send(RepairCommand::SwitchModel {
        session_id: ROOT.to_owned(),
        model: String::new(),
    });
    // Barrier: an explicit switch on another session is applied after the
    // fallback failed on the first one.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
    });
    wait_for_model(&harness.store, OTHER, "beta").await;

    let root = harness
        .store
        .get_session(ROOT)
        .await
        .expect("session")
        .expect("exists");
    assert_eq!(root["model"]["id"], "alpha");
    let events = harness.sink.data("session.model.selected");
    assert_eq!(events.len(), 1, "only the explicit switch emitted an event");
    assert_eq!(events[0]["sessionID"], OTHER);
}
