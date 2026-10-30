//! Manual (`POST /api/session/:sid/compact`) and observer compaction delivery.
//!
//! A `compaction` inbox row must reach the actor, run exactly once and be
//! consumed whether the model succeeds or fails. Every test is offline: temp
//! SQLite, temp config, scripted model.

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
    RuntimeConfig,
};
use oc_store::{Delivery, MessageQuery, NewInbox, NewMessage, NewSession, Order, Store};
use oc_tools::ToolRegistry;
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
    Delayed {
        events: Vec<LlmEvent>,
        delay: Duration,
    },
    /// Simulated provider failure (HTTP 500); the request never yields a stream.
    Fail,
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
                Script::Delayed { events, delay } => {
                    tokio::time::sleep(delay).await;
                    Ok(stream::iter(events).boxed())
                }
                Script::Fail => Err(LlmError::Http {
                    status: 500,
                    message: "scripted provider failure".to_owned(),
                    retry_after_ms: None,
                }),
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
}

struct AllowAll;

impl PermissionHandler for AllowAll {
    fn resolve(&self, _request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        Box::pin(async { PermissionDecision::Allow })
    }
}

struct Harness {
    _root: TempDir,
    runtime: Runtime,
    store: Arc<Store>,
    sink: TestSink,
    model: Arc<ScriptedModel>,
    session: String,
}

impl Harness {
    async fn new(scripts: Vec<Script>) -> Self {
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
        let session = "ses_manual_compact".to_owned();
        store
            .create_session(NewSession {
                id: session.clone(),
                parent_id: None,
                project_id: "prj_manual_compact".to_owned(),
                directory: path(&work),
                title: Some("manual compaction".to_owned()),
                agent: Some("build".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
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
            permissions: Arc::new(AllowAll),
        };
        let runtime = Runtime::new_with_model_client(config, Arc::clone(&model) as _);
        Self {
            _root: root,
            runtime,
            store,
            sink,
            model,
            session,
        }
    }

    /// Enqueues the same row the HTTP route writes for `POST .../compact`.
    async fn enqueue_compaction(&self) -> String {
        let id = MessageId::new().into_inner();
        self.store
            .enqueue(NewInbox {
                id: id.clone(),
                session_id: self.session.clone(),
                kind: "compaction".to_owned(),
                payload: json!({
                    "time": {"created": oc_types::now_millis()},
                    "status": "pending",
                    "reason": "manual"
                }),
                delivery: Delivery::Queue,
            })
            .await
            .expect("compaction enqueued");
        id
    }

    async fn inbox(&self) -> Vec<Value> {
        self.store.list_inbox(&self.session).await.expect("inbox")
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

    async fn compaction_rows(&self) -> Vec<Value> {
        self.messages()
            .await
            .into_iter()
            .filter(|message| message.get("type").and_then(Value::as_str) == Some("compaction"))
            .collect()
    }

    /// Bounded wait: the inbox row disappears and a finished compaction row
    /// exists.
    async fn wait_compaction_settled(&self, timeout: Duration) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let rows = self.compaction_rows().await;
            let inbox = self.inbox().await;
            let finished = rows.iter().any(|row| {
                matches!(
                    row.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                )
            });
            if inbox.is_empty() && finished {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; inbox={inbox:?} compaction={rows:?}"
            );
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

#[tokio::test]
async fn manual_compaction_consumes_row_and_completes() {
    let harness = Harness::new(vec![text_script("## Objective\nhandoff summary")]).await;
    let id = harness.enqueue_compaction().await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");

    let rows = harness
        .wait_compaction_settled(Duration::from_secs(10))
        .await;
    assert!(
        harness.inbox().await.is_empty(),
        "inbox row {id} must be consumed"
    );
    assert_eq!(rows.len(), 1, "exactly one compaction message");
    let compaction = &rows[0];
    assert_eq!(compaction["status"], "completed");
    assert_eq!(compaction["reason"], "manual");
    assert!(
        compaction["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("handoff summary"))
    );

    // The compaction request carries the transcript plus the summary prompt and
    // no tools, and never produces a fake assistant turn.
    let requests = harness.model.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert_eq!(requests[0].options.tool_choice.as_deref(), Some("none"));
    assert!(
        harness
            .messages()
            .await
            .iter()
            .all(|message| message.get("type").and_then(Value::as_str) != Some("assistant"))
    );

    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("session idle after compaction");
    assert!(
        harness
            .sink
            .kinds()
            .iter()
            .all(|kind| !kind.starts_with("session.execution.")),
        "compaction must not fake an execution event"
    );
}

#[tokio::test]
async fn manual_compaction_is_active_while_running() {
    let harness = Harness::new(vec![Script::Delayed {
        events: vec![
            LlmEvent::TextStart,
            LlmEvent::TextDelta {
                delta: "## Objective\nslow handoff".to_owned(),
            },
            LlmEvent::TextEnd {
                text: "## Objective\nslow handoff".to_owned(),
            },
            LlmEvent::Finish {
                reason: "stop".to_owned(),
                usage: usage(),
            },
        ],
        delay: Duration::from_millis(300),
    }])
    .await;
    harness.enqueue_compaction().await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");

    let deadline = Instant::now() + Duration::from_secs(5);
    while harness.model.calls() == 0 {
        assert!(Instant::now() < deadline, "compaction never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        harness.runtime.active().contains(&harness.session),
        "session must report active during compaction"
    );

    let rows = harness
        .wait_compaction_settled(Duration::from_secs(10))
        .await;
    assert_eq!(rows[0]["status"], "completed");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("session idle after compaction");
}

#[tokio::test]
async fn failed_compaction_consumes_row_without_retrying() {
    let harness = Harness::new(vec![Script::Fail]).await;
    let id = harness.enqueue_compaction().await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");

    let rows = harness
        .wait_compaction_settled(Duration::from_secs(10))
        .await;
    assert!(
        harness.inbox().await.is_empty(),
        "inbox row {id} must be consumed"
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "failed");
    assert!(
        rows[0]["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
    assert_eq!(
        harness.model.calls(),
        1,
        "the failed compaction must not retry"
    );

    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("session idle after failed compaction");

    // Give a hypothetical retry loop time to enqueue or call again.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(harness.inbox().await.is_empty());
    assert_eq!(harness.model.calls(), 1);
    assert_eq!(harness.compaction_rows().await.len(), 1);
    assert!(
        !harness
            .sink
            .kinds()
            .iter()
            .any(|kind| kind == "session.execution.failed")
    );
}

#[tokio::test]
async fn compaction_keeps_the_active_turn_in_the_next_context() {
    let harness = Harness::new(vec![
        text_script("## Objective\nhandoff summary"),
        text_script("after response"),
    ])
    .await;
    let now = oc_types::now_millis();
    harness
        .store
        .append_message(NewMessage {
            id: "msg_keep_user".to_owned(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            data: json!({"text": "please keep this context", "time": {"created": now}}),
            time_created: now,
        })
        .await
        .expect("keep user");
    harness
        .store
        .append_message(NewMessage {
            id: "msg_keep_assistant".to_owned(),
            session_id: harness.session.clone(),
            kind: "assistant".to_owned(),
            data: json!({
                "time": {"created": now, "streamed": now, "completed": now},
                "content": [{"type": "text", "text": "kept answer"}],
                "finish": "stop"
            }),
            time_created: now,
        })
        .await
        .expect("keep assistant");

    harness.enqueue_compaction().await;
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");
    let rows = harness
        .wait_compaction_settled(Duration::from_secs(10))
        .await;
    let completed = rows
        .iter()
        .find(|row| row["status"] == "completed")
        .expect("completed compaction");
    assert_eq!(completed["keepID"], "msg_keep_user", "{completed}");

    let inbox = MessageId::new().into_inner();
    harness
        .store
        .enqueue(NewInbox {
            id: inbox,
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "next step", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("next prompt");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify next");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    let last = requests.last().expect("next request");
    let texts: Vec<String> = last
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|part| part.as_text().map(str::to_owned))
        .collect();
    let summary = texts
        .iter()
        .position(|text| text.contains("handoff summary"))
        .expect("summary present");
    let kept = texts
        .iter()
        .position(|text| text.contains("please keep this context"))
        .expect("retained prompt");
    let next = texts
        .iter()
        .position(|text| text.contains("next step"))
        .expect("next prompt");
    assert!(summary < kept && kept < next, "{texts:?}");
}
