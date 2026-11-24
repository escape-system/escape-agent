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
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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
    /// Direct command sender for the legacy tests; `None` when the harness owns
    /// the real [`oc_runtime::RepairChannel`] executor instead.
    sender: Option<UnboundedSender<RepairCommand>>,
    /// The runtime's own executor half, used by the receipt tests so `apply`
    /// round-trips through the repair worker.
    executor: Option<Arc<dyn RepairExecutor>>,
    runtime: Runtime,
}

impl Harness {
    async fn new(config_content: Value) -> Self {
        Self::with_model(config_content, Arc::new(EmptyModel)).await
    }

    /// A harness that injects commands directly (no executor half).
    async fn with_model(config_content: Value, model: Arc<dyn oc_runtime::ModelClient>) -> Self {
        let (sender, repairs) = tokio::sync::mpsc::unbounded_channel();
        let harness = Self::build(config_content, model, repairs).await;
        Self {
            sender: Some(sender),
            executor: None,
            ..harness
        }
    }

    /// A harness whose repair worker is fed by the real `RepairChannel`
    /// executor, so `apply` waits for the runtime's receipt.
    async fn with_receipts(config_content: Value) -> Self {
        Self::with_receipts_model(config_content, Arc::new(EmptyModel)).await
    }

    async fn with_receipts_model(
        config_content: Value,
        model: Arc<dyn oc_runtime::ModelClient>,
    ) -> Self {
        let (executor, repairs) = oc_runtime::RepairChannel::new();
        let harness = Self::build(config_content, model, repairs).await;
        Self {
            sender: None,
            executor: Some(executor),
            ..harness
        }
    }

    async fn build(
        config_content: Value,
        model: Arc<dyn oc_runtime::ModelClient>,
        repairs: UnboundedReceiver<RepairCommand>,
    ) -> Self {
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

        let catalog = Arc::new(oc_llm::CatalogCell::from_arc(Arc::new(
            Catalog::from_snapshot(CATALOG).expect("catalog"),
        )));
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
            sender: None,
            executor: None,
            runtime,
        }
    }

    /// Apply one repair verdict through the runtime executor and return the
    /// receipt it answered with.
    async fn apply(
        &self,
        verdict: &oc_observer::ObserverVerdict,
    ) -> Result<RepairOutcome, ObserverError> {
        self.executor
            .as_ref()
            .expect("this harness owns the real repair channel")
            .apply(verdict)
            .await
    }

    fn send(&self, command: RepairCommand) {
        self.sender
            .as_ref()
            .expect("this harness injects commands")
            .send(command)
            .expect("repair command");
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
            agent: Some("engineer".to_owned()),
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
        reply: None,
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
        reply: None,
    });
    // A later explicit switch on another session is the barrier that proves
    // the repost command was processed.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
        reply: None,
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
        reply: None,
    });
    // The running prompt is no longer in the inbox, so only the idle check can
    // stop a duplicate execution.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
        reply: None,
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
        reply: None,
    });
    // A valid switch on another session is the barrier that proves the ghost
    // command was processed before we read ROOT.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
        reply: None,
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
        reply: None,
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
        reply: None,
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
        reply: None,
    });
    // Barrier: an explicit switch on another session is applied after the
    // fallback failed on the first one.
    harness.send(RepairCommand::SwitchModel {
        session_id: OTHER.to_owned(),
        model: "test/beta".to_owned(),
        reply: None,
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

// ---------------------------------------------------------------------------
// Receipts: the runtime's own verdicts, driven through `RepairChannel`.
//
// These assert the *producer* half of the repair contract that
// `oc-observer/tests/repair_receipts.rs` consumes. Before the receipt landed,
// `ChannelExecutor::apply` returned `applied: true` as soon as the command was
// queued, so every one of these cases was reported to the observer as a repair
// that happened.
// ---------------------------------------------------------------------------

/// A repair verdict shaped like the observer's, for one target.
fn repair_verdict(
    target: &str,
    kind: oc_observer::RepairKind,
    args: Value,
) -> oc_observer::ObserverVerdict {
    oc_observer::ObserverVerdict {
        action: oc_observer::ObserverAction::Repair,
        target: Some(target.to_owned()),
        reason: "the target looks stuck".to_owned(),
        message: None,
        repair: Some(oc_observer::Repair {
            kind: Some(kind),
            args,
        }),
        goal_evidence: Vec::new(),
    }
}

/// Assert the receipt is a refusal that names a reason.
fn assert_refused(outcome: &RepairOutcome, because: &str) {
    assert!(
        !outcome.applied,
        "a repair that did not happen must not report applied: true ({because}): {outcome:?}"
    );
    let detail = outcome
        .detail
        .as_deref()
        .unwrap_or_else(|| panic!("a refusal must carry a reason ({because}): {outcome:?}"));
    assert!(
        !detail.trim().is_empty(),
        "the refusal reason must not be blank ({because})"
    );
}

/// R1: the exact defect from `docs/18-status.md:403-408`.
///
/// A `switch_model` naming a model the catalog cannot run is refused by the
/// runtime. Before the receipt, `apply` answered `applied: true` for it while
/// the model was correctly not persisted — the audit row and
/// `observer.repaired` overstated success.
#[tokio::test]
async fn r1_a_catalog_unknown_switch_model_receipt_is_not_applied() {
    let harness =
        Harness::with_receipts(json!({"observer": {"escalation_model": "test/alpha"}})).await;

    let outcome = harness
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::SwitchModel,
            json!({"model": "test/ghost"}),
        ))
        .await
        .expect("the runtime answers a refusal, not an absent answer");

    assert_refused(&outcome, "catalog-unknown model");
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("not in the catalog")),
        "the refusal must name the catalog rejection: {outcome:?}"
    );
    // The model was already correctly not persisted; the point is that the
    // receipt now agrees with that.
    let row = harness
        .store
        .get_session(ROOT)
        .await
        .expect("session")
        .expect("root");
    assert_eq!(row["model"]["id"], "alpha");
}

/// R2: `cancel_tool` with nothing to cancel.
///
/// The worker used to log the zero count and return success; the receipt must
/// be `applied: false`.
#[tokio::test]
async fn r2_a_cancel_tool_that_cancelled_nothing_is_not_applied() {
    let harness = Harness::with_receipts(json!({})).await;

    let outcome = harness
        .apply(&repair_verdict(
            "call_never_started",
            oc_observer::RepairKind::CancelTool,
            json!({}),
        ))
        .await
        .expect("the runtime answers a refusal");

    assert_refused(&outcome, "nothing to cancel");
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("found no running tool call")),
        "the refusal must say no tool call matched: {outcome:?}"
    );
}

/// R3a: `restart_subagent` on a child with no user prompt to replay.
#[tokio::test]
async fn r3a_a_restart_subagent_without_a_prompt_is_not_applied() {
    let harness = Harness::with_receipts(json!({})).await;
    let child = create_child(&harness, ROOT, "ses_receipt_child", ".").await;

    let outcome = harness
        .apply(&repair_verdict(
            &child,
            oc_observer::RepairKind::RestartSubagent,
            json!({"note": "the child stalled"}),
        ))
        .await
        .expect("the runtime answers a refusal");

    assert_refused(&outcome, "no prompt to replay");
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no user prompt to replay")),
        "the refusal must say why the restart was skipped: {outcome:?}"
    );
}

/// R3b: `repost` with nothing to re-deliver.
#[tokio::test]
async fn r3b_a_repost_without_a_prompt_is_not_applied() {
    let harness = Harness::with_receipts(json!({})).await;

    let outcome = harness
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::Repost,
            json!({}),
        ))
        .await
        .expect("the runtime answers a refusal");

    assert_refused(&outcome, "no prompt to re-deliver");
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no user prompt to re-deliver")),
        "the refusal must say why the repost was skipped: {outcome:?}"
    );
}

/// R3c: `repost` while the session already has pending input.
///
/// A different skip path from R3b — the prompt exists, but delivering it would
/// duplicate queued work.
#[tokio::test]
async fn r3c_a_repost_with_pending_input_is_not_applied() {
    let harness = Harness::with_receipts(json!({})).await;
    append_user(&harness, ROOT, "msg_receipt_pending", "latest prompt").await;
    enqueue_user(&harness, ROOT, "already waiting").await;

    let outcome = harness
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::Repost,
            json!({}),
        ))
        .await
        .expect("the runtime answers a refusal");

    assert_refused(&outcome, "pending input");
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("already has pending input")),
        "the refusal must say the session is not idle: {outcome:?}"
    );
}

/// R4: a repair the runtime really performs still reports success.
///
/// `switch_model` to a catalog model is applied synchronously — the session row
/// is patched and `session.model.selected` is published — so the receipt must
/// stay `applied: true` and be backed by that committed state.
#[tokio::test]
async fn r4_an_applied_switch_model_still_reports_success() {
    let harness = Harness::with_receipts(json!({})).await;

    let outcome = harness
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::SwitchModel,
            json!({"model": "test/beta"}),
        ))
        .await
        .expect("the runtime applied the switch");

    assert!(
        outcome.applied,
        "a switch the runtime performed must report success: {outcome:?}"
    );
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("switched") && detail.contains("test/beta")),
        "the success detail must name the switch: {outcome:?}"
    );
    wait_for_model(&harness.store, ROOT, "beta").await;
    assert_eq!(
        harness.sink.count("session.model.selected"),
        1,
        "the success receipt must be backed by the published event"
    );
}

/// R4b: a queued compaction is still a success, and a duplicate is refused.
///
/// The runtime owns the durable `compaction` inbox row and the wake-up, so
/// queueing one is a real repair; a second request while the first is queued
/// cannot do anything and must be refused.
#[tokio::test]
async fn r4b_a_queued_compaction_is_applied_and_a_duplicate_is_not() {
    let harness = Harness::with_receipts(json!({})).await;
    // Pre-queue a compaction row without waking the actor, so the guard is
    // observable instead of racing the runner.
    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: ROOT.to_owned(),
            kind: "compaction".to_owned(),
            payload: json!({"status": "pending", "reason": "observer"}),
            delivery: Delivery::Queue,
        })
        .await
        .expect("compaction row");

    let outcome = harness
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::CompactContext,
            json!({}),
        ))
        .await
        .expect("the runtime answers a refusal");

    assert_refused(&outcome, "a compaction is already queued");
    let queued = harness
        .store
        .list_inbox(ROOT)
        .await
        .expect("inbox")
        .iter()
        .filter(|row| row.get("type").and_then(Value::as_str) == Some("compaction"))
        .count();
    assert_eq!(queued, 1, "the refusal must not queue a second compaction");
}

/// R5: a consumer that drops the receipt without answering is a failure.
///
/// The receiver is dropped before the worker can answer, so `apply` must return
/// `Err` — never a fabricated `applied: true`.
#[tokio::test]
async fn r5_a_dropped_receipt_is_an_executor_error() {
    let (executor, mut repairs) = oc_runtime::RepairChannel::new();
    // Stand in for a worker that dequeues and dies: drop every receipt.
    let drain = tokio::spawn(async move {
        while let Some(mut command) = repairs.recv().await {
            drop(command.take_reply());
        }
    });

    let outcome = executor
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::CancelTool,
            json!({"tool": "hang_probe"}),
        ))
        .await;

    drain.abort();
    let error = outcome.expect_err("a receipt that never arrives must not be a success");
    assert!(
        matches!(error, ObserverError::Executor(_)),
        "a dropped receipt must surface as Executor: {error:?}"
    );
    assert!(
        error.to_string().contains("without answering"),
        "the error must say the worker never answered: {error}"
    );
}

/// R5b: no worker at all (receiver dropped up front) is an executor error too.
#[tokio::test]
async fn r5b_a_stopped_worker_is_an_executor_error() {
    let (executor, repairs) = oc_runtime::RepairChannel::new();
    drop(repairs);

    let outcome = executor
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::CompactContext,
            json!({}),
        ))
        .await;

    let error = outcome.expect_err("a stopped worker cannot have applied a repair");
    assert!(
        error.to_string().contains("worker is stopped"),
        "the error must say the worker is gone: {error}"
    );
}

/// R6: `request_budget_extension` answers with the grant, not the request.
///
/// A session with no running turn cannot be extended, so the receipt is a
/// refusal naming that — previously the executor's short-circuit answered
/// `applied: true` with `detail: "budget_request"` for every request.
#[tokio::test]
async fn r6_a_budget_extension_without_a_running_turn_is_not_applied() {
    let harness = Harness::with_receipts(json!({})).await;

    let outcome = harness
        .apply(&repair_verdict(
            ROOT,
            oc_observer::RepairKind::RequestBudgetExtension,
            json!({"steps": 5}),
        ))
        .await
        .expect("the runtime answers a refusal");

    assert_refused(&outcome, "no running turn to extend");
    assert!(
        outcome
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("no running turn")),
        "the refusal must name the missing turn: {outcome:?}"
    );
}
