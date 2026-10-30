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
