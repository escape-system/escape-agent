//! Shared fixtures for the live-delivery tests: gated scripted model, sink and
//! temp-store harness.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use oc_tools::{Tool, ToolRegistry};
use oc_types::{EventEnvelope, MessageId};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};

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

/// One tool call emitted by a scripted step.
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
}

pub(crate) fn call(id: &str, name: &str, input: Value) -> Call {
    Call {
        id: id.to_owned(),
        name: name.to_owned(),
        input,
    }
}

/// One scripted provider response. `gate` makes the step wait until the test
/// releases it, which is how rows are enqueued between steps.
pub(crate) enum Step {
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

pub(crate) struct GateModel {
    pub(crate) steps: Mutex<VecDeque<Step>>,
    pub(crate) calls: Mutex<Vec<LlmRequest>>,
    pub(crate) started: mpsc::UnboundedSender<usize>,
    pub(crate) count: AtomicUsize,
}

impl GateModel {
    pub(crate) fn new(steps: Vec<Step>) -> (Arc<Self>, mpsc::UnboundedReceiver<usize>) {
        let (started, started_rx) = mpsc::unbounded_channel();
        let model = Arc::new(Self {
            steps: Mutex::new(steps.into()),
            calls: Mutex::new(Vec::new()),
            started,
            count: AtomicUsize::new(0),
        });
        (model, started_rx)
    }

    pub(crate) fn requests(&self) -> Vec<LlmRequest> {
        self.calls.lock().expect("requests").clone()
    }

    pub(crate) fn calls(&self) -> usize {
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
pub(crate) struct TestSink {
    pub(crate) events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl EventSink for TestSink {
    fn publish(&self, event: EventEnvelope) {
        self.events.lock().expect("events").push(event);
    }
}

impl TestSink {
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

/// Test-only tool that blocks until a pending inbox row for the calling
/// session carries `needle`. It models "the background child finished before
/// the parent's next step", without relying on scheduling luck.
pub(crate) struct Harness {
    pub(crate) _root: TempDir,
    pub(crate) runtime: Runtime,
    pub(crate) store: Arc<Store>,
    pub(crate) sink: TestSink,
    pub(crate) model: Arc<GateModel>,
    pub(crate) session: String,
    pub(crate) started: tokio::sync::Mutex<mpsc::UnboundedReceiver<usize>>,
}

impl Harness {
    pub(crate) async fn new(
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

    pub(crate) async fn enqueue(&self, kind: &str, text: &str, delivery: Delivery) -> String {
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

    pub(crate) async fn prompt(&self, text: &str) {
        self.enqueue("user", text, Delivery::Steer).await;
        self.runtime.notify(&self.session).await.expect("notify");
    }

    pub(crate) async fn idle(&self) {
        self.runtime
            .wait_idle(&self.session, Duration::from_secs(15))
            .await
            .expect("idle");
    }

    /// Index of the next provider call that started.
    pub(crate) async fn next_started(&self) -> usize {
        self.started
            .lock()
            .await
            .recv()
            .await
            .expect("the model was called")
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

    pub(crate) async fn inbox(&self) -> Vec<Value> {
        self.store.list_inbox(&self.session).await.expect("inbox")
    }

    /// How many `session.inbox.delivered` events this session published for
    /// `inbox_id`. The sink is shared, so child sessions are filtered out.
    pub(crate) fn delivery_count(&self, inbox_id: &str) -> usize {
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

pub(crate) fn tool_events(calls: &[Call]) -> Vec<LlmEvent> {
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

/// Answers with a sentence that names the needle when the request already
/// carries it, so the persisted answer proves what the model saw.
pub(crate) fn echo_events(needle: &str, request: &LlmRequest) -> Vec<LlmEvent> {
    let text = if contains_text(request, needle) {
        format!("final answer mentions {needle}")
    } else {
        format!("final answer without {needle}")
    };
    text_events(&text)
}

/// All text the request carries, joined, so ordering between two needles can
/// be asserted on the exact conversation the model saw.
pub(crate) fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(ContentPart::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn contains_text(request: &LlmRequest, needle: &str) -> bool {
    request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .filter_map(ContentPart::as_text)
            .any(|text| text.contains(needle))
    })
}

pub(crate) fn message_text(message: &Value) -> String {
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

pub(crate) fn of_type<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .filter(|message| message.get("type").and_then(Value::as_str) == Some(kind))
        .collect()
}
