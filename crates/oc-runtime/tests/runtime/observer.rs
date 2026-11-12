//! Observer wiring: engine construction, notices and repair commands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_llm::LlmEvent;
use oc_observer::{
    FlightStrip, GoalSnapshot, GoalStatus, GoalStore, ObserverError, RepairExecutor, RepairOutcome,
};
use oc_runtime::{ObserverBinding, RepairChannel, RepairCommand, StoreGoalStore};
use oc_store::NewGoal;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::support::*;

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

struct GatePermission {
    asked: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl oc_runtime::PermissionHandler for GatePermission {
    fn resolve(
        &self,
        _request: oc_runtime::PermissionAsk,
    ) -> futures_util::future::BoxFuture<'static, oc_runtime::PermissionDecision> {
        let asked = Arc::clone(&self.asked);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            asked.notify_one();
            release.notified().await;
            oc_runtime::PermissionDecision::Allow
        })
    }
}

#[tokio::test]
async fn permission_wait_does_not_open_flight_strip_until_approved() {
    let asked = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let harness = Harness::new(
        vec![
            tool_script(
                "call_perm_wait",
                json!({"path": "hello.txt", "expectation": expectation()}),
            ),
            text_script("done after approval"),
        ],
        Arc::new(GatePermission {
            asked: Arc::clone(&asked),
            release: Arc::clone(&release),
        }),
        json!({}),
        Some(json!([{"action": "read", "resource": "*", "effect": "ask"}])),
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

    harness.enqueue("read with ask").await;
    runtime.notify(&harness.session).await.expect("notify");
    tokio::time::timeout(Duration::from_secs(5), asked.notified())
        .await
        .expect("permission asked");

    assert!(
        engine.registry().active(&harness.session).is_empty(),
        "flight strip must not open while blocked on user permission approval"
    );

    release.notify_one();
    runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let recent = engine.registry().recent(&harness.session, 10);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].tool_call_id, "call_perm_wait");
    assert_eq!(recent[0].state, oc_observer::StripState::Ok);
}

struct HangingTool {
    started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl oc_tools::Tool for HangingTool {
    fn name(&self) -> &'static str {
        "hang_probe"
    }

    fn description(&self) -> &'static str {
        "tool that waits indefinitely until cancelled"
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _ctx: &oc_tools::ToolContext,
        _input: Value,
    ) -> Result<oc_tools::ToolResult, oc_tools::ToolError> {
        self.started.notify_one();
        futures_util::future::pending::<Result<oc_tools::ToolResult, oc_tools::ToolError>>().await
    }
}

struct HangingProvider {
    started: Arc<tokio::sync::Notify>,
}

impl oc_runtime::ToolProvider for HangingProvider {
    fn tools(
        &self,
        _session: &oc_types::SessionInfo,
        _agent: &str,
    ) -> Vec<Arc<dyn oc_tools::Tool>> {
        vec![Arc::new(HangingTool {
            started: Arc::clone(&self.started),
        })]
    }
}

#[tokio::test]
async fn cancel_tool_cancels_the_running_tool_without_interrupting_the_turn() {
    let started = Arc::new(tokio::sync::Notify::new());
    let harness = Harness::new(
        vec![
            named_tool_script(
                "hang_probe",
                "call_hang",
                json!({"expectation": expectation()}),
            ),
            text_script("recovered after tool cancel"),
        ],
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
    runtime.add_tool_provider(Arc::new(HangingProvider {
        started: Arc::clone(&started),
    }));
    runtime.start().await.expect("start");

    harness.enqueue("run hang_probe").await;
    runtime.notify(&harness.session).await.expect("notify");
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("hang_probe started");

    sender
        .send(RepairCommand::CancelTool {
            tool_call_id: "call_hang".to_owned(),
        })
        .expect("send cancel_tool");

    runtime
        .wait_idle(&harness.session, Duration::from_secs(10))
        .await
        .expect("idle");

    let failures = harness.sink.data("session.tool.failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["error"]["type"], "ToolCancelledError");
    assert!(harness.sink.has("session.execution.succeeded"));
    assert!(!harness.sink.has("session.execution.interrupted"));
}

/// Root `step` values the observer model actually received, in call order.
///
/// `RootInput.step` is always serialized, so the rendered prompt carries the
/// wire value the observer judged with.
fn observed_steps(model: &FixedObserverModel) -> Vec<u32> {
    const KEY: &str = "\"step\":";
    model
        .prompts()
        .iter()
        .filter_map(|prompt| {
            let start = prompt.find(KEY)? + KEY.len();
            let digits: String = prompt[start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .collect()
}

#[tokio::test]
async fn observer_sees_the_live_turn_step() {
    // Two requests: a tool step, then a slow text step. The tool has no
    // `expectation`, so no flight strip opens and the busy root satisfies the
    // stall condition (`stall_seconds: 0`) on every tick; that makes the
    // observer evaluate repeatedly *during* the turn instead of once at its
    // start, which is what exercises the live counter.
    let harness = Harness::new(
        vec![
            named_tool_script("read", "call_step", json!({"path": "hello.txt"})),
            delayed_text_script("done", Duration::from_millis(500)),
        ],
        Arc::new(AllowAll),
        json!({}),
        None,
    )
    .await;
    let model = FixedObserverModel::text(&json!({"action": "none"}));
    let engine = Arc::new(oc_observer::ObserverEngine::new(
        oc_observer::ObserverConfig {
            tick_ms: 20,
            min_interval_ms: 0,
            stall_seconds: 0,
            max_calls_per_min: 60,
            ..oc_observer::ObserverConfig::default()
        },
        Arc::new(oc_observer::SystemClock),
        Arc::clone(&model) as Arc<dyn oc_observer::ObserverModel>,
        Arc::new(NeverExecutor),
        Arc::new(RecordingNotifier::default()),
    ));
    let (_, repairs) = RepairChannel::new();
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

    let steps = observed_steps(&model);
    assert!(
        !steps.is_empty(),
        "observer never evaluated during the turn"
    );
    // Before the live counter every evaluation carried the hardcoded `0`; the
    // regression property is that a busy turn is judged at its real step.
    assert_eq!(
        steps.iter().copied().max(),
        Some(1),
        "the observer must have judged the second, in-flight request as step 1: {steps:?}"
    );
    assert!(
        steps.iter().all(|step| *step <= 1),
        "the step of a two-request turn can never exceed 1: {steps:?}"
    );
}
