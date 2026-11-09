//! Nested-spawn integration tests.
//!
//! A `general` child may spawn its own children (bounded by
//! `subagents.max_depth`/`allow_spawn`), while `explore` keeps its built-in
//! `subagent` deny and stays read-only. Everything runs offline against a temp
//! SQLite database and a scripted model client.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use futures_util::stream::{self, StreamExt};
use oc_config::{ConfigService, LoadOptions};
use oc_llm::{
    Catalog, LlmClient, LlmError, LlmEvent, LlmRequest, LlmStream, Role, SharedLlmClient, Usage,
};
use oc_runtime::{
    EventSink, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler, Runtime,
    RuntimeConfig,
};
use oc_store::{Delivery, MessageQuery, NewInbox, NewSession, NewSessionTree, Order, Store};
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

/// One scripted provider response. Each call pops the next script.
enum Script {
    Events(Vec<LlmEvent>),
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
                Script::Delayed { events, delay } => Ok(stream::iter(events)
                    .then(move |event| async move {
                        tokio::time::sleep(delay).await;
                        event
                    })
                    .boxed()),
            }
        })
    }
}

#[derive(Default)]
struct TestSink {
    events: Mutex<Vec<EventEnvelope>>,
}

impl EventSink for TestSink {
    fn publish(&self, event: EventEnvelope) {
        self.events.lock().expect("events").push(event);
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
    model: Arc<ScriptedModel>,
    session: String,
}

impl Harness {
    async fn new(scripts: Vec<Script>, subagents: Value) -> Self {
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
                .with_env(
                    "OPENCODE_CONFIG_CONTENT",
                    json!({ "subagents": subagents }).to_string(),
                ),
        )
        .expect("config loads");

        let store = Arc::new(
            Store::open(root.path().join("db.sqlite"))
                .await
                .expect("store opens"),
        );
        let session = "ses_nested_root".to_owned();
        store
            .create_session_with_tree(
                NewSession {
                    id: session.clone(),
                    parent_id: None,
                    project_id: "prj_nested_test".to_owned(),
                    directory: path(&work),
                    title: Some("nested root".to_owned()),
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
            .expect("root session");

        let model = ScriptedModel::new(scripts);
        let config = RuntimeConfig {
            store: Arc::clone(&store),
            config: Arc::new(config),
            catalog: Arc::new(Catalog::from_snapshot(CATALOG).expect("catalog")),
            client: Arc::new(SharedLlmClient::new(LlmClient::new().expect("client"))),
            tools: Arc::new(ToolRegistry::with_defaults()),
            sink: Arc::new(TestSink::default()),
            permissions: Arc::new(AllowAll),
        };
        let runtime = Runtime::new_with_model_client(config, Arc::clone(&model) as _);
        Self {
            _root: root,
            runtime,
            store,
            model,
            session,
        }
    }

    async fn prompt(&self) {
        self.store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: self.session.clone(),
                kind: "user".to_owned(),
                payload: json!({"text": "start", "time": {"created": oc_types::now_millis()}}),
                delivery: Delivery::Steer,
            })
            .await
            .expect("enqueued");
        self.runtime.notify(&self.session).await.expect("notify");
        self.runtime
            .wait_idle(&self.session, Duration::from_secs(15))
            .await
            .expect("idle");
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
    Script::Delayed {
        events: vec![
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
        ],
        delay,
    }
}

fn tool_script(call_id: &str, agent: &str, prompt: &str) -> Script {
    let input = json!({
        "agent": agent,
        "description": prompt.to_lowercase(),
        "prompt": prompt,
        "background": false,
    });
    tool_input_script(call_id, input)
}

fn tool_input_script(call_id: &str, input: Value) -> Script {
    let text = input.to_string();
    Script::Events(vec![
        LlmEvent::StepStart,
        LlmEvent::TextStart,
        LlmEvent::TextDelta {
            delta: "spawning".to_owned(),
        },
        LlmEvent::TextEnd {
            text: "spawning".to_owned(),
        },
        LlmEvent::ToolInputStart {
            id: call_id.to_owned(),
            name: "subagent".to_owned(),
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
            name: "subagent".to_owned(),
            input,
        },
        LlmEvent::Finish {
            reason: "tool_calls".to_owned(),
            usage: usage(),
        },
    ])
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

fn tool_names(request: &LlmRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

fn nested_config(max_depth: u32) -> Value {
    json!({
        "enabled": true,
        "max_depth": max_depth,
        "max_children_per_session": 8,
        "allow_spawn": true,
        "default_agent": "general",
        "background_default": false,
    })
}

#[tokio::test]
async fn general_spawns_a_grandchild_while_explore_stays_read_only() {
    let harness = Harness::new(
        vec![
            tool_script("call_root", "general", "SPAWN_GRANDCHILD"),
            tool_script("call_child", "explore", "GRANDCHILD_OK"),
            text_script("GRANDCHILD_DONE"),
            text_script("CHILD_DONE"),
            text_script("ROOT_DONE"),
        ],
        nested_config(3),
    )
    .await;
    harness.prompt().await;

    let children = harness
        .store
        .children(&harness.session)
        .await
        .expect("children");
    assert_eq!(children.len(), 1, "the root spawned one general child");
    let child = &children[0];
    assert_eq!(child.agent, "general");
    assert_eq!(child.depth, 1);
    assert_eq!(child.parent_id.as_deref(), Some(harness.session.as_str()));
    assert!(child.spawn_allowed, "general may spawn under allow_spawn");

    let grandchildren = harness
        .store
        .children(&child.id)
        .await
        .expect("grandchildren");
    assert_eq!(
        grandchildren.len(),
        1,
        "the general child spawned a grandchild"
    );
    let grandchild = &grandchildren[0];
    assert_eq!(grandchild.agent, "explore");
    assert_eq!(grandchild.depth, 2, "the grandchild sits at depth 2");
    assert_eq!(grandchild.parent_id.as_deref(), Some(child.id.as_str()));
    assert_eq!(grandchild.root_id, harness.session);

    // Request order is deterministic: the foreground spawns are awaited.
    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        5,
        "root spawn, child spawn, grandchild answer, child answer, root answer"
    );
    let child_names = tool_names(&requests[1]);
    assert!(
        child_names.iter().any(|name| name == "subagent"),
        "general's built-in deny is gone, so `subagent` reaches its request: {child_names:?}"
    );
    let grandchild_names = tool_names(&requests[2]);
    assert!(
        !grandchild_names.iter().any(|name| name == "subagent"),
        "explore keeps its deny and never sees `subagent`: {grandchild_names:?}"
    );
    assert!(
        grandchild_names.iter().any(|name| name == "read"),
        "explore still sees its read-only tools: {grandchild_names:?}"
    );
    assert!(
        request_tool_result(&requests[3], 0).contains("GRANDCHILD_DONE"),
        "the child received the grandchild's final text"
    );
    let root_result = request_tool_result(&requests[4], 0);
    assert!(root_result.contains("CHILD_DONE"), "{root_result}");
    assert!(!root_result.contains("GRANDCHILD_DONE"), "{root_result}");
}

#[tokio::test]
async fn interrupt_during_foreground_subagent_wait_cancels_the_child() {
    let harness = Harness::new(
        vec![
            tool_script("call_root", "general", "SLOW_CHILD"),
            delayed_text_script("CHILD_DONE", Duration::from_secs(30)),
            text_script("ROOT_DONE"),
        ],
        nested_config(3),
    )
    .await;

    harness
        .store
        .enqueue(NewInbox {
            id: MessageId::new().into_inner(),
            session_id: harness.session.clone(),
            kind: "user".to_owned(),
            payload: json!({"text": "start", "time": {"created": oc_types::now_millis()}}),
            delivery: Delivery::Steer,
        })
        .await
        .expect("enqueued");
    harness
        .runtime
        .notify(&harness.session)
        .await
        .expect("notify");

    let deadline = Instant::now() + Duration::from_secs(10);
    let child = loop {
        let children = harness
            .store
            .children(&harness.session)
            .await
            .expect("children");
        if let Some(child) = children.first() {
            break child.id.clone();
        }
        assert!(Instant::now() < deadline, "the root never spawned a child");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // Sync on the child's provider call, not just on the actor existing. A stop
    // that lands after the actor is registered but before it promotes the
    // queued prompt now withdraws that row (see `withdraw_queued_input`), so
    // the child would never run a turn and would legitimately have no idle row.
    // `requests` counts the root call first and the child call second.
    while harness.model.requests().len() < 2 {
        assert!(Instant::now() < deadline, "the child never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    harness
        .runtime
        .interrupt(&harness.session)
        .await
        .expect("interrupt the root");
    harness
        .runtime
        .wait_idle(&harness.session, Duration::from_secs(5))
        .await
        .expect("the root must settle");
    // Without the cancellation guard this waits the child's full 30s stream.
    harness
        .runtime
        .wait_idle(&child, Duration::from_secs(3))
        .await
        .expect("cancelling the parent must interrupt the child");

    let idle = harness
        .store
        .list_messages(MessageQuery {
            session_id: child.clone(),
            limit: 5,
            order: Order::Desc,
            cursor: None,
            kind: Some("idle".to_owned()),
        })
        .await
        .expect("child idle messages");
    assert_eq!(
        idle.items
            .first()
            .and_then(|message| message.get("outcome"))
            .and_then(Value::as_str),
        Some("interrupted")
    );

    let inbox = harness
        .store
        .list_inbox(&harness.session)
        .await
        .expect("inbox");
    assert!(
        !inbox
            .iter()
            .any(|item| item["payload"]["metadata"]["source"] == "subagent"),
        "a cancelled wait must not deliver a synthetic child result: {inbox:?}"
    );
}

#[tokio::test]
async fn max_depth_refuses_the_next_spawn_without_creating_a_session() {
    let harness = Harness::new(
        vec![
            tool_script("call_root", "general", "SPAWN_TOO_DEEP"),
            tool_script("call_child", "explore", "GRANDCHILD_TOO_DEEP"),
            text_script("CHILD_DEPTH_BLOCKED"),
            text_script("ROOT_DEPTH_BLOCKED"),
        ],
        nested_config(1),
    )
    .await;
    harness.prompt().await;

    let children = harness
        .store
        .children(&harness.session)
        .await
        .expect("children");
    assert_eq!(children.len(), 1, "depth 1 is still allowed");
    let child = &children[0];
    assert_eq!(child.depth, 1);
    assert!(
        harness
            .store
            .children(&child.id)
            .await
            .expect("grandchildren")
            .is_empty(),
        "the depth-2 spawn must not create a session"
    );

    let requests = harness.model.requests();
    assert_eq!(
        requests.len(),
        4,
        "root spawn, child spawn, child answer, root answer"
    );
    let error = request_tool_result(&requests[2], 0);
    assert!(error.contains("SpawnForbiddenError"), "{error}");
    assert!(
        error.contains("spawn depth 2 exceeds max depth 1"),
        "the depth error names both numbers: {error}"
    );
    assert!(
        request_tool_result(&requests[3], 0).contains("CHILD_DEPTH_BLOCKED"),
        "the failed spawn is reported back to the root as a tool result"
    );
}
