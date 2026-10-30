//! Live mid-turn delivery.
//!
//! Inbox rows with `delivery: "steer"` must promote at step boundaries of a
//! running turn, while `delivery: "queue"` rows wait for the next execution.
//! Everything runs offline against a temp SQLite database and a scripted model
//! whose steps can hold a gate so the test can enqueue between them.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{
    Catalog, ContentPart, LlmClient, LlmError, LlmEvent, LlmRequest, LlmStream, SharedLlmClient,
    Usage,
};
use oc_runtime::{
    EventSink, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler, Runtime,
    RuntimeConfig,
};
use oc_store::{Delivery, MessageQuery, NewInbox, NewSession, NewSessionTree, Order, Store};
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::{EventEnvelope, MessageId};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};

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

/// One tool call emitted by a scripted step.
struct Call {
    id: String,
    name: String,
    input: Value,
}

fn call(id: &str, name: &str, input: Value) -> Call {
    Call {
        id: id.to_owned(),
        name: name.to_owned(),
        input,
    }
}

/// One scripted provider response. `gate` makes the step wait until the test
/// releases it, which is how rows are enqueued between steps.
enum Step {
    Tools {
        gate: Option<Arc<Notify>>,
        calls: Vec<Call>,
    },
    Echo {
        gate: Option<Arc<Notify>>,
        needle: String,
    },
    Text {
        text: String,
    },
}

struct GateModel {
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<LlmRequest>>,
    started: mpsc::UnboundedSender<usize>,
    count: AtomicUsize,
}

impl GateModel {
    fn new(steps: Vec<Step>) -> (Arc<Self>, mpsc::UnboundedReceiver<usize>) {
        let (started, started_rx) = mpsc::unbounded_channel();
        let model = Arc::new(Self {
            steps: Mutex::new(steps.into()),
            calls: Mutex::new(Vec::new()),
            started,
            count: AtomicUsize::new(0),
        });
        (model, started_rx)
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.calls.lock().expect("requests").clone()
    }

    fn calls(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl ModelClient for GateModel {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        let index = {
            let mut calls = self.calls.lock().expect("requests");
            let index = calls.len();
            calls.push(request.clone());
            index
        };
        self.count.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .lock()
            .expect("scripts")
            .pop_front()
            .unwrap_or(Step::Text {
                text: String::new(),
            });
        let started = self.started.clone();
        Box::pin(async move {
            let _ = started.send(index);
            let gate = match &step {
                Step::Tools { gate, .. } | Step::Echo { gate, .. } => gate.clone(),
                Step::Text { .. } => None,
            };
            if let Some(gate) = gate {
                gate.notified().await;
            }
            let events = match &step {
                Step::Tools { calls, .. } => tool_events(calls),
                Step::Echo { needle, .. } => echo_events(needle, &request),
                Step::Text { text } => text_events(text),
            };
            Ok(stream::iter(events).boxed())
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

/// Test-only tool that blocks until a pending inbox row for the calling
/// session carries `needle`. It models "the background child finished before
/// the parent's next step", without relying on scheduling luck.
struct WaitForDelivery {
    store: Arc<Store>,
    needle: String,
}

#[async_trait::async_trait]
impl Tool for WaitForDelivery {
    fn name(&self) -> &'static str {
        "wait_for_delivery"
    }

    fn description(&self) -> &'static str {
        "Test-only: blocks until a pending inbox row contains the needle."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let items = self
                .store
                .list_inbox(&ctx.session_id)
                .await
                .map_err(|error| ToolError::new(error.to_string()))?;
            let found = items.iter().any(|item| {
                item.pointer("/payload/text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(&self.needle))
            });
            if found {
                return Ok(ToolResult::text("delivery pending"));
            }
            if Instant::now() >= deadline {
                return Err(ToolError::new(format!(
                    "timed out waiting for an inbox row containing {:?}",
                    self.needle
                )));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct Harness {
    _root: TempDir,
    runtime: Runtime,
    store: Arc<Store>,
    sink: TestSink,
    model: Arc<GateModel>,
    session: String,
    started: tokio::sync::Mutex<mpsc::UnboundedReceiver<usize>>,
}

impl Harness {
    async fn new(
        steps: Vec<Step>,
        config_content: Value,
        extra_tools: impl FnOnce(&Arc<Store>) -> Vec<Arc<dyn Tool>>,
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
        let session = "ses_steer_root".to_owned();
        store
            .create_session_with_tree(
                NewSession {
                    id: session.clone(),
                    parent_id: None,
                    project_id: "prj_steer_test".to_owned(),
                    directory: path(&work),
                    title: Some("steer test".to_owned()),
                    agent: Some("build".to_owned()),
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
            .expect("session created");

        let (model, started) = GateModel::new(steps);
        let mut registry = ToolRegistry::with_defaults();
        for tool in extra_tools(&store) {
            registry.register(tool);
        }
        let sink = TestSink::default();
        let runtime_config = RuntimeConfig {
            store: Arc::clone(&store),
            config: Arc::new(config),
            catalog: Arc::new(Catalog::from_snapshot(CATALOG).expect("catalog")),
            client: Arc::new(SharedLlmClient::new(LlmClient::new().expect("client"))),
            tools: Arc::new(registry),
            sink: Arc::new(sink.clone()),
            permissions: Arc::new(AllowAll),
        };
        let runtime = Runtime::new_with_model_client(runtime_config, Arc::clone(&model) as _);
        Self {
            _root: root,
            runtime,
            store,
            sink,
            model,
            session,
            started: tokio::sync::Mutex::new(started),
        }
    }

    async fn enqueue(&self, kind: &str, text: &str, delivery: Delivery) -> String {
        let id = MessageId::new().into_inner();
        self.store
            .enqueue(NewInbox {
                id: id.clone(),
                session_id: self.session.clone(),
                kind: kind.to_owned(),
                payload: json!({"text": text, "time": {"created": oc_types::now_millis()}}),
                delivery,
            })
            .await
            .expect("enqueued");
        id
    }

    async fn prompt(&self, text: &str) {
        self.enqueue("user", text, Delivery::Steer).await;
        self.runtime.notify(&self.session).await.expect("notify");
    }

    async fn idle(&self) {
        self.runtime
            .wait_idle(&self.session, Duration::from_secs(15))
            .await
            .expect("idle");
    }

    /// Index of the next provider call that started.
    async fn next_started(&self) -> usize {
        self.started
            .lock()
            .await
            .recv()
            .await
            .expect("the model was called")
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

    async fn inbox(&self) -> Vec<Value> {
        self.store.list_inbox(&self.session).await.expect("inbox")
    }

    /// How many `session.inbox.delivered` events this session published for
    /// `inbox_id`. The sink is shared, so child sessions are filtered out.
    fn delivery_count(&self, inbox_id: &str) -> usize {
        self.sink
            .data("session.inbox.delivered")
            .iter()
            .filter(|data| {
                data["sessionID"] == Value::String(self.session.clone())
                    && data["inboxID"].as_str() == Some(inbox_id)
            })
            .count()
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

fn tool_events(calls: &[Call]) -> Vec<LlmEvent> {
    let mut events = vec![
        LlmEvent::StepStart,
        LlmEvent::TextStart,
        LlmEvent::TextDelta {
            delta: "working".to_owned(),
        },
        LlmEvent::TextEnd {
            text: "working".to_owned(),
        },
    ];
    for call in calls {
        let text = call.input.to_string();
        events.push(LlmEvent::ToolInputStart {
            id: call.id.clone(),
            name: call.name.clone(),
        });
        events.push(LlmEvent::ToolInputDelta {
            id: call.id.clone(),
            delta: text.clone(),
        });
        events.push(LlmEvent::ToolInputEnd {
            id: call.id.clone(),
            text,
        });
        events.push(LlmEvent::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            input: call.input.clone(),
        });
    }
    events.push(LlmEvent::Finish {
        reason: "tool_calls".to_owned(),
        usage: usage(),
    });
    events
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

/// Answers with a sentence that names the needle when the request already
/// carries it, so the persisted answer proves what the model saw.
fn echo_events(needle: &str, request: &LlmRequest) -> Vec<LlmEvent> {
    let text = if contains_text(request, needle) {
        format!("final answer mentions {needle}")
    } else {
        format!("final answer without {needle}")
    };
    text_events(&text)
}

fn contains_text(request: &LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .filter_map(ContentPart::as_text)
            .any(|text| text.contains(needle))
    })
}

fn message_text(message: &Value) -> String {
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    message
        .get("content")
        .and_then(Value::as_array)
        .map_or_else(String::new, |parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
}

fn of_type<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some(kind))
        .collect()
}

#[tokio::test]
async fn steer_row_is_promoted_at_the_next_step_boundary() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: Some(Arc::clone(&gate)),
                calls: vec![call("call_read", "read", json!({"path": "hello.txt"}))],
            },
            Step::Echo {
                gate: None,
                needle: "SYNTHETIC_STEER".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start the turn").await;
    assert_eq!(
        harness.next_started().await,
        0,
        "the first step holds the gate"
    );

    let steer = harness
        .enqueue(
            "synthetic",
            "SYNTHETIC_STEER: the child finished",
            Delivery::Steer,
        )
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(requests.len(), 2, "one provider call per step");
    assert!(
        !contains_text(&requests[0], "SYNTHETIC_STEER"),
        "the steer cannot precede its enqueue"
    );
    assert!(
        contains_text(&requests[1], "SYNTHETIC_STEER"),
        "step two must carry the promoted steer"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 2);
    assert!(
        message_text(assistants[1]).contains("mentions SYNTHETIC_STEER"),
        "the final answer references the promoted note: {}",
        message_text(assistants[1])
    );
    assert_eq!(
        of_type(&messages, "synthetic").len(),
        1,
        "the steer becomes exactly one message"
    );

    let delivered = harness.delivery_count(&steer);
    assert_eq!(delivered, 1, "one delivery event for the steer");
    assert!(harness.inbox().await.is_empty(), "the inbox drained");
}

#[tokio::test]
async fn queue_row_waits_for_the_next_execution() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: Some(Arc::clone(&gate)),
                calls: vec![call("call_read", "read", json!({"path": "hello.txt"}))],
            },
            Step::Echo {
                gate: None,
                needle: "QUEUED_NOTE".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "QUEUED_NOTE".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start the turn").await;
    assert_eq!(harness.next_started().await, 0);

    let queued = harness
        .enqueue("user", "QUEUED_NOTE: run after this turn", Delivery::Queue)
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "two turn steps plus the queued execution"
    );
    assert!(
        !contains_text(&requests[0], "QUEUED_NOTE"),
        "a queue row is never promoted mid-turn"
    );
    assert!(
        !contains_text(&requests[1], "QUEUED_NOTE"),
        "the second step of the turn must not see the queue row"
    );
    assert!(
        contains_text(&requests[2], "QUEUED_NOTE"),
        "the next execution promotes the queue row"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 3, "two turn steps plus queued turn");
    assert!(
        message_text(assistants[1]).contains("without QUEUED_NOTE"),
        "the running turn finished without the queue row: {}",
        message_text(assistants[1])
    );
    assert!(
        message_text(assistants[2]).contains("mentions QUEUED_NOTE"),
        "the queued execution saw its own prompt"
    );
    assert_eq!(
        of_type(&messages, "user")
            .iter()
            .filter(|message| message_text(message).contains("QUEUED_NOTE"))
            .count(),
        1,
        "the queue row becomes exactly one user message"
    );
    assert_eq!(
        harness.delivery_count(&queued),
        1,
        "the queue row is delivered once, by the next execution"
    );
    assert!(harness.inbox().await.is_empty());
}

#[tokio::test]
async fn steer_arriving_during_the_final_stream_waits_for_the_next_execution() {
    let gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Echo {
                gate: Some(Arc::clone(&gate)),
                needle: "LATE_STEER".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "LATE_STEER".to_owned(),
            },
        ],
        json!({}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("single step").await;
    assert_eq!(harness.next_started().await, 0);

    // The turn has no step boundary left, so this row cannot join it.
    let late = harness
        .enqueue("synthetic", "LATE_STEER: arrived too late", Delivery::Steer)
        .await;
    gate.notify_one();
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        2,
        "the finished turn plus one follow-up execution"
    );
    assert!(
        !contains_text(&requests[0], "LATE_STEER"),
        "the finished stream cannot see the late steer"
    );
    assert!(
        contains_text(&requests[1], "LATE_STEER"),
        "the next execution delivers the late steer"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 2);
    assert!(message_text(assistants[0]).contains("without LATE_STEER"));
    assert!(message_text(assistants[1]).contains("mentions LATE_STEER"));
    assert_eq!(harness.delivery_count(&late), 1, "delivered exactly once");
    assert!(harness.inbox().await.is_empty());
}

#[tokio::test]
async fn background_subagent_completion_reaches_the_running_turn() {
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![
                    call(
                        "call_spawn",
                        "subagent",
                        json!({
                            "agent": "general",
                            "description": "quick child",
                            "prompt": "child work",
                            "background": true,
                        }),
                    ),
                    call("call_wait", "wait_for_delivery", json!({})),
                ],
            },
            Step::Text {
                text: "child done work".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "state=\"completed\"".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |store| {
            vec![Arc::new(WaitForDelivery {
                store: Arc::clone(store),
                needle: "<subagent".to_owned(),
            })]
        },
    )
    .await;

    harness.prompt("start").await;
    harness.idle().await;

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        3,
        "parent step one, the background child, then parent step two"
    );
    assert!(
        contains_text(&requests[2], "<subagent")
            && contains_text(&requests[2], "state=\"completed\"")
            && contains_text(&requests[2], "child done work"),
        "the step after the spawn carries the completion envelope"
    );

    let messages = harness.messages().await;
    let assistants = of_type(&messages, "assistant");
    assert_eq!(assistants.len(), 2, "parent steps only");
    assert!(
        message_text(assistants[1]).contains("mentions state=\"completed\""),
        "the turn's final answer saw the completion: {}",
        message_text(assistants[1])
    );
    let synthetic = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic.len(),
        1,
        "the completion is delivered exactly once"
    );
    assert!(message_text(synthetic[0]).contains("<subagent"));
    let synthetic_id = synthetic[0]["id"].as_str().expect("synthetic id");
    assert_eq!(
        harness.delivery_count(synthetic_id),
        1,
        "one delivery event for the completion"
    );
    assert_eq!(harness.model.calls(), 3);
    assert!(
        harness.inbox().await.is_empty(),
        "the completion was consumed mid-turn, not left for settle"
    );
}

/// Tool results are [`ContentPart::ToolResult`], so [`contains_text`] cannot see
/// them; the detach contract is about what the tool told the model.
fn contains_any_text(request: &LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message.content.iter().any(|part| match part {
            ContentPart::Text { text } => text.contains(needle),
            ContentPart::ToolResult { content, .. } => content.contains(needle),
            _ => false,
        })
    })
}

impl Harness {
    /// Poll the parent transcript for the first synthetic message containing
    /// `needle`, so no assertion depends on scheduling luck.
    async fn wait_for_synthetic(&self, needle: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let messages = self.messages().await;
            let found = of_type(&messages, "synthetic")
                .into_iter()
                .find(|message| message_text(message).contains(needle))
                .cloned();
            if let Some(found) = found {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no synthetic message containing {needle:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test]
async fn background_request_detaches_a_foreground_subagent_wait() {
    let child_gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "call_spawn",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "slow child",
                        "prompt": "child work",
                        "background": false,
                    }),
                )],
            },
            Step::Echo {
                gate: Some(Arc::clone(&child_gate)),
                needle: "child work".to_owned(),
            },
            Step::Text {
                text: "parent continued".to_owned(),
            },
            Step::Text {
                text: "saw child completion".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(harness.next_started().await, 0, "the parent step starts");
    assert_eq!(
        harness.next_started().await,
        1,
        "the child is running while the parent waits"
    );

    let requested = Instant::now();
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("background request"),
        "the parent execution is running"
    );

    // The foreground wait must abandon within one poll tick and let the turn
    // continue, although the child is still gated.
    let next = tokio::time::timeout(Duration::from_secs(1), harness.next_started())
        .await
        .expect("the detach returned the parent's next step within 1s");
    assert_eq!(next, 2, "the parent turn continued to its next step");
    assert!(requested.elapsed() < Duration::from_secs(1));

    let requests = harness.model.requests();
    assert!(
        contains_any_text(&requests[2], "\"status\":\"running\""),
        "the tool result reports a running child: {:?}",
        requests[2].messages
    );

    child_gate.notify_one();
    let synthetic = harness.wait_for_synthetic("<subagent").await;
    harness.idle().await;

    assert!(
        message_text(&synthetic).contains("state=\"completed\"")
            && message_text(&synthetic).contains("mentions child work"),
        "the child finished on its own and delivered exactly once: {}",
        message_text(&synthetic)
    );
    let messages = harness.messages().await;
    let synthetic_messages = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic_messages.len(),
        1,
        "exactly one completion reaches the parent"
    );
    let assistants = of_type(&messages, "assistant");
    assert!(
        assistants
            .iter()
            .any(|message| message_text(message).contains("parent continued")),
        "the detached turn finished without blocking"
    );
    assert!(
        assistants
            .iter()
            .any(|message| message_text(message).contains("saw child completion")),
        "the completion drove a later execution"
    );
}

#[tokio::test]
async fn background_request_on_an_idle_session_returns_false() {
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "spawn_one",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "first child",
                        "prompt": "first work",
                        "background": false,
                    }),
                )],
            },
            Step::Text {
                text: "first child answer".to_owned(),
            },
            Step::Text {
                text: "first parent answer".to_owned(),
            },
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "spawn_two",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "second child",
                        "prompt": "second work",
                        "background": false,
                    }),
                )],
            },
            Step::Text {
                text: "second child answer".to_owned(),
            },
            Step::Text {
                text: "second parent answer".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |_| Vec::new(),
    )
    .await;

    // No actor exists yet.
    assert!(
        !harness
            .runtime
            .request_background(&harness.session)
            .expect("background request"),
        "an unknown session has nothing to detach"
    );

    harness.prompt("first").await;
    harness.idle().await;
    assert!(
        !harness
            .runtime
            .request_background(&harness.session)
            .expect("background request"),
        "an idle session has nothing to detach"
    );

    // The next execution must still wait for a foreground child.
    harness.prompt("second").await;
    harness.idle().await;

    let requests = harness.model.requests();
    assert!(
        contains_any_text(&requests[2], "state=\"completed\"")
            && contains_any_text(&requests[2], "first child answer"),
        "the first foreground wait returned the child's final text"
    );
    assert!(
        contains_any_text(&requests[5], "state=\"completed\"")
            && contains_any_text(&requests[5], "second child answer"),
        "the idle request did not corrupt the next execution"
    );
    let messages = harness.messages().await;
    assert!(
        of_type(&messages, "synthetic").is_empty(),
        "foreground completions are tool results, not synthetic messages"
    );
}

#[tokio::test]
async fn explicit_background_spawn_ignores_a_background_request() {
    let child_gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![
                    call(
                        "call_spawn",
                        "subagent",
                        json!({
                            "agent": "general",
                            "description": "background child",
                            "prompt": "child work",
                            "background": true,
                        }),
                    ),
                    call("call_wait", "wait_for_delivery", json!({})),
                ],
            },
            Step::Echo {
                gate: Some(Arc::clone(&child_gate)),
                needle: "child work".to_owned(),
            },
            Step::Echo {
                gate: None,
                needle: "state=\"completed\"".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |store| {
            vec![Arc::new(WaitForDelivery {
                store: Arc::clone(store),
                needle: "<subagent".to_owned(),
            })]
        },
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(harness.next_started().await, 0);
    assert_eq!(harness.next_started().await, 1, "the child is running");

    // The parent's step is blocked in `wait_for_delivery`, not in a subagent
    // wait; the request must not add a second watcher.
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("background request")
    );

    child_gate.notify_one();
    harness.idle().await;

    let messages = harness.messages().await;
    let synthetic = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic.len(),
        1,
        "the explicit background spawn still delivers exactly once"
    );
    assert!(message_text(synthetic[0]).contains("state=\"completed\""));
    let requests = harness.model.requests();
    assert!(
        contains_any_text(&requests[2], "\"status\":\"running\""),
        "the background tool result is unchanged"
    );
}

#[tokio::test]
async fn a_second_background_request_after_consumption_has_no_effect() {
    let child_gate = Arc::new(Notify::new());
    let parent_gate = Arc::new(Notify::new());
    let harness = Harness::new(
        vec![
            Step::Tools {
                gate: None,
                calls: vec![call(
                    "call_spawn",
                    "subagent",
                    json!({
                        "agent": "general",
                        "description": "slow child",
                        "prompt": "child work",
                        "background": false,
                    }),
                )],
            },
            Step::Echo {
                gate: Some(Arc::clone(&child_gate)),
                needle: "child work".to_owned(),
            },
            Step::Echo {
                gate: Some(Arc::clone(&parent_gate)),
                needle: "PARENT_SECOND_STEP".to_owned(),
            },
            Step::Text {
                text: "saw child completion".to_owned(),
            },
        ],
        json!({"subagents": {"background_default": false}}),
        |_| Vec::new(),
    )
    .await;

    harness.prompt("start").await;
    assert_eq!(harness.next_started().await, 0);
    assert_eq!(harness.next_started().await, 1);
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("background request")
    );

    // The parent is still running while its second step is gated.
    let next = tokio::time::timeout(Duration::from_secs(1), harness.next_started())
        .await
        .expect("the detach returned the parent's next step within 1s");
    assert_eq!(next, 2);
    assert!(
        harness
            .runtime
            .request_background(&harness.session)
            .expect("second background request"),
        "the parent execution is still running"
    );

    parent_gate.notify_one();
    harness.idle().await;
    child_gate.notify_one();
    harness.wait_for_synthetic("<subagent").await;
    harness.idle().await;

    let messages = harness.messages().await;
    let synthetic = of_type(&messages, "synthetic");
    assert_eq!(
        synthetic.len(),
        1,
        "the second request did not spawn another watcher"
    );
    assert!(message_text(synthetic[0]).contains("state=\"completed\""));
    assert_eq!(
        harness.model.calls(),
        4,
        "spawn, child, parent step two, completion turn"
    );
}
