//! Auto-title generation, `agent.request`, `snapshots: false`, `tool_output`, and
//! `session.metadata.environment` forwarding in the runtime turn loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream;
use oc_llm::{LlmError, LlmRequest, LlmStream};
use oc_runtime::{ModelClient, Runtime};
use oc_store::{Delivery, NewInbox, NewSession, SessionPatch};
use oc_types::MessageId;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::support::*;

struct MockGate {
    entered: Notify,
    release: Notify,
    is_entered: AtomicBool,
    is_released: AtomicBool,
}

impl MockGate {
    fn new() -> Self {
        Self {
            entered: Notify::new(),
            release: Notify::new(),
            is_entered: AtomicBool::new(false),
            is_released: AtomicBool::new(false),
        }
    }

    async fn wait_entered(&self) {
        while !self.is_entered.load(Ordering::SeqCst) {
            self.entered.notified().await;
        }
    }

    fn release(&self) {
        self.is_released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

struct TitleAwareModel {
    title_reply: String,
    inner: Arc<ScriptedModel>,
    title_requests: std::sync::Mutex<Vec<LlmRequest>>,
    title_gate: Option<Arc<MockGate>>,
}

impl TitleAwareModel {
    fn new(title_reply: &str, main_replies: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            title_reply: title_reply.to_owned(),
            inner: ScriptedModel::new(main_replies),
            title_requests: std::sync::Mutex::new(Vec::new()),
            title_gate: None,
        })
    }

    fn with_title_gate(
        title_reply: &str,
        main_replies: Vec<Script>,
        gate: Arc<MockGate>,
    ) -> Arc<Self> {
        Arc::new(Self {
            title_reply: title_reply.to_owned(),
            inner: ScriptedModel::new(main_replies),
            title_requests: std::sync::Mutex::new(Vec::new()),
            title_gate: Some(gate),
        })
    }

    fn title_requests(&self) -> Vec<LlmRequest> {
        self.title_requests.lock().expect("title requests").clone()
    }

    fn main_requests(&self) -> Vec<LlmRequest> {
        self.inner.requests()
    }
}

impl ModelClient for TitleAwareModel {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        let is_title = request
            .system
            .iter()
            .any(|part| part.contains("name conversations so the user can find them later"));
        if is_title {
            self.title_requests
                .lock()
                .expect("title requests")
                .push(request);
            let title_reply = self.title_reply.clone();
            let gate = self.title_gate.clone();
            return Box::pin(async move {
                if let Some(gate) = gate {
                    gate.is_entered.store(true, Ordering::SeqCst);
                    gate.entered.notify_waiters();
                    while !gate.is_released.load(Ordering::SeqCst) {
                        gate.release.notified().await;
                    }
                }
                let Script::Events(events) = text_script(&title_reply) else {
                    unreachable!("text_script builds events");
                };
                Ok(stream::iter(events).boxed())
            });
        }

        self.inner.stream(request)
    }
}

async fn create_session_with_title(
    harness: &Harness,
    id: &str,
    parent_id: Option<&str>,
    title: Option<&str>,
    metadata: Option<Value>,
) {
    harness
        .store
        .create_session(NewSession {
            id: id.to_owned(),
            parent_id: parent_id.map(str::to_owned),
            project_id: "prj_runtime_test".to_owned(),
            directory: path(&harness.work),
            title: title.map(str::to_owned),
            agent: Some("engineer".to_owned()),
            model: Some(json!({"providerID": "test", "id": "alpha"})),
            metadata,
            permissions: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("session created");
}

async fn enqueue_prompt(harness: &Harness, session: &str, text: &str) {
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
        .expect("enqueued");
}

#[tokio::test]
async fn untitled_root_session_auto_renames_and_persists_title() {
    let base = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let model = TitleAwareModel::new(
        "<think>thinking</think>\nTitle: \"Fix auth token refresh.\"",
        vec![
            text_script("I fixed it 1."),
            text_script("I fixed it 2."),
            text_script("I fixed it 3."),
        ],
    );
    let runtime = Runtime::new_with_model_client(base.config.clone(), Arc::clone(&model) as _);

    for (sid, initial_title) in [
        ("ses_untitled_none", None),
        ("ses_untitled_default", Some("New session")),
        (
            "ses_untitled_iso",
            Some("New session - 2026-11-25T19:43:42.123Z"),
        ),
    ] {
        create_session_with_title(&base, sid, None, initial_title, None).await;
        enqueue_prompt(&base, sid, "Please fix auth token refresh").await;
        runtime.notify(sid).await.expect("notify");
        runtime
            .wait_idle(sid, Duration::from_secs(10))
            .await
            .expect("idle");

        let stored = base
            .store
            .get_session(sid)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(
            stored["title"].as_str(),
            Some("Fix auth token refresh"),
            "session {sid} should be auto-renamed"
        );
        let events = base.store.events_after(sid, 0, 100).await.expect("events");
        let renamed: Vec<&Value> = events
            .iter()
            .filter(|ev| ev.kind == "session.renamed")
            .map(|ev| &ev.data)
            .collect();
        assert_eq!(renamed.len(), 1, "events for {sid}: {events:?}");
        assert_eq!(renamed[0]["title"], "Fix auth token refresh");
    }

    let title_reqs = model.title_requests();
    assert_eq!(title_reqs.len(), 3);
    assert!(
        title_reqs[0].tools.is_empty(),
        "title generation must not expose tools"
    );
}

#[tokio::test]
async fn child_and_custom_titled_sessions_skip_auto_title() {
    let base = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let model = TitleAwareModel::new(
        "Should Not Appear",
        vec![text_script("a"), text_script("b")],
    );
    let runtime = Runtime::new_with_model_client(base.config.clone(), Arc::clone(&model) as _);

    create_session_with_title(&base, "ses_parent_root", None, Some("Parent work"), None).await;
    create_session_with_title(
        &base,
        "ses_child_untitled",
        Some("ses_parent_root"),
        None,
        None,
    )
    .await;

    for sid in ["ses_parent_root", "ses_child_untitled"] {
        enqueue_prompt(&base, sid, "hello").await;
        runtime.notify(sid).await.expect("notify");
        runtime
            .wait_idle(sid, Duration::from_secs(10))
            .await
            .expect("idle");
        let events = base.store.events_after(sid, 0, 100).await.expect("events");
        assert!(
            events.iter().all(|ev| ev.kind != "session.renamed"),
            "unexpected session.renamed on {sid}: {events:?}"
        );
    }

    assert!(model.title_requests().is_empty());
    assert_eq!(model.main_requests().len(), 2);
}

#[tokio::test]
async fn concurrent_manual_rename_is_not_overwritten_by_auto_title() {
    let base = Harness::new(vec![], Arc::new(AllowAll), json!({}), None).await;
    let gate = Arc::new(MockGate::new());
    let model = TitleAwareModel::with_title_gate(
        "Auto Generated Name",
        vec![text_script("main reply")],
        Arc::clone(&gate),
    );
    let runtime = Runtime::new_with_model_client(base.config.clone(), Arc::clone(&model) as _);

    let sid = "ses_manual_race";
    create_session_with_title(&base, sid, None, None, None).await;
    enqueue_prompt(&base, sid, "investigate cache").await;
    runtime.notify(sid).await.expect("notify");

    gate.wait_entered().await;
    base.store
        .patch_session(
            sid,
            SessionPatch {
                title: Some("User Chosen Name".to_owned()),
                ..SessionPatch::default()
            },
        )
        .await
        .expect("manual patch");
    gate.release();

    runtime
        .wait_idle(sid, Duration::from_secs(10))
        .await
        .expect("idle");

    let stored = base
        .store
        .get_session(sid)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(stored["title"].as_str(), Some("User Chosen Name"));
    let events = base.store.events_after(sid, 0, 100).await.expect("events");
    assert!(
        events.iter().all(|ev| ev.kind != "session.renamed"),
        "auto-title must not overwrite manual title: {events:?}"
    );
}

#[tokio::test]
async fn configured_small_model_routes_auto_title() {
    let base = Harness::new(
        vec![],
        Arc::new(AllowAll),
        json!({
            "small_model": "test/mini"
        }),
        None,
    )
    .await;
    let mut catalog = base.config.catalog.current().as_ref().clone();
    catalog
        .apply_config(&json!({
            "test": {
                "models": {
                    "mini": {
                        "name": "Mini",
                        "type": "chat"
                    }
                }
            }
        }))
        .expect("apply mini overlay");
    base.config.catalog.replace(catalog);

    let model = TitleAwareModel::new("Mini Named Title", vec![text_script("answered")]);
    let runtime = Runtime::new_with_model_client(base.config.clone(), Arc::clone(&model) as _);

    let sid = "ses_small_model_title";
    create_session_with_title(&base, sid, None, None, None).await;
    enqueue_prompt(&base, sid, "check small_model routing").await;
    runtime.notify(sid).await.expect("notify");
    runtime
        .wait_idle(sid, Duration::from_secs(10))
        .await
        .expect("idle");

    let title_reqs = model.title_requests();
    assert_eq!(title_reqs.len(), 1);
    assert_eq!(title_reqs[0].model.model.id, "mini");
    assert_eq!(model.main_requests()[0].model.model.id, "alpha");
}

#[tokio::test]
async fn agent_request_overrides_and_snapshots_false_work() {
    let harness = Harness::new(
        vec![
            named_tool_script(
                "write",
                "call_write",
                json!({
                    "path": "note.txt",
                    "content": "hello",
                    "expectation": expectation(),
                }),
            ),
            text_script("all done"),
        ],
        Arc::new(AllowAll),
        json!({
            "snapshots": false,
            "agents": {
                "engineer": {
                    "request": {
                        "headers": { "x-agent-header": "abc" },
                        "body": {
                            "temperature": 0.25,
                            "top_p": 0.85,
                            "seed": 42
                        }
                    }
                }
            }
        }),
        None,
    )
    .await;

    let sid = "ses_agent_snap";
    create_session_with_title(&harness, sid, None, Some("explicit config session"), None).await;
    enqueue_prompt(&harness, sid, "write file").await;
    harness.runtime.notify(sid).await.expect("notify");
    harness
        .runtime
        .wait_idle(sid, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    assert!(!requests.is_empty());
    assert_eq!(requests[0].options.temperature, Some(0.25));
    assert_eq!(requests[0].options.top_p, Some(0.85));
    assert_eq!(
        requests[0]
            .options
            .headers
            .get("x-agent-header")
            .map(String::as_str),
        Some("abc")
    );
    assert_eq!(requests[0].model.body().get("seed"), Some(&json!(42)));

    let snapshots = harness
        .store
        .snapshots_after(sid, -1)
        .await
        .expect("snapshots");
    assert!(
        snapshots.is_empty(),
        "expected zero snapshots when snapshots: false, got {snapshots:?}"
    );
}

#[tokio::test]
async fn session_environment_and_tool_output_limits_reach_tools() {
    let long_cmd = format!(
        "echo $CUSTOM_SESSION_ENV; echo line1; echo line2; printf '%s' '{}'",
        "y".repeat(420)
    );
    let harness = Harness::new(
        vec![
            named_tool_script(
                "shell",
                "call_shell_env",
                json!({
                    "command": "printf 'env=%s\\n' \"$CUSTOM_SESSION_ENV\"",
                    "expectation": expectation(),
                }),
            ),
            named_tool_script(
                "shell",
                "call_shell_bounds",
                json!({
                    "command": long_cmd,
                    "expectation": expectation(),
                }),
            ),
            text_script("all done"),
        ],
        Arc::new(AllowAll),
        json!({
            "tool_output": {
                "max_lines": 2,
                "max_bytes": 300
            }
        }),
        None,
    )
    .await;

    let sid = "ses_tool_cfg";
    create_session_with_title(
        &harness,
        sid,
        None,
        Some("explicit tool session"),
        Some(json!({
            "environment": {
                "CUSTOM_SESSION_ENV": "env_value_123"
            }
        })),
    )
    .await;
    enqueue_prompt(&harness, sid, "run shells").await;
    harness.runtime.notify(sid).await.expect("notify");
    harness
        .runtime
        .wait_idle(sid, Duration::from_secs(10))
        .await
        .expect("idle");

    let requests = harness.model.requests();
    let env_result = request_tool_result(&requests[1], 0);
    assert!(
        env_result.contains("env=env_value_123"),
        "expected session env in shell output, got: {env_result:?}"
    );

    let bounded_result = request_tool_result(&requests[2], 1);
    assert!(
        bounded_result.contains("truncated"),
        "expected shell/runtime truncation notice for max_lines=2 / max_bytes=300, got: {bounded_result:?}"
    );
}
