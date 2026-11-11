//! Deterministic engine tests: fake clock, scripted model, recorded repairs.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use oc_observer::{
    BoxFuture, BudgetState, Clock, FlightStrip, GoalSnapshot, HostFlag, ObserverConfig,
    ObserverEngine, ObserverError, ObserverModel, ObserverNotice, ObserverNotifier, ObserverRun,
    ObserverVerdict, RepairExecutor, RepairKind, RepairOutcome, RootContext, SkipReason,
    StripState, TreeRow,
};

#[derive(Debug, Default)]
struct FakeClock(AtomicI64);

impl FakeClock {
    fn set(&self, now: i64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct ScriptedModel {
    responses: Mutex<VecDeque<Result<String, ObserverError>>>,
    calls: AtomicUsize,
    last_input: Mutex<Option<String>>,
}

impl ScriptedModel {
    fn new(responses: Vec<Result<String, ObserverError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            calls: AtomicUsize::new(0),
            last_input: Mutex::new(None),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_input(&self) -> Option<String> {
        self.last_input.lock().expect("input lock").clone()
    }
}

impl ObserverModel for ScriptedModel {
    fn judge<'a>(
        &'a self,
        input: &'a str,
        _cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_input.lock().expect("input lock") = Some(input.to_owned());
        let next = self
            .responses
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| Err(ObserverError::Model("script exhausted".to_owned())));
        Box::pin(async move { next })
    }
}

#[derive(Default)]
struct RecordingExecutor {
    calls: Mutex<Vec<ObserverVerdict>>,
}

impl RepairExecutor for RecordingExecutor {
    fn apply<'a>(
        &'a self,
        verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        self.calls
            .lock()
            .expect("executor lock")
            .push(verdict.clone());
        Box::pin(async {
            Ok(RepairOutcome {
                applied: true,
                detail: Some("recorded".to_owned()),
            })
        })
    }
}

#[derive(Default)]
struct RecordingNotifier {
    notices: Mutex<Vec<ObserverNotice>>,
}

impl RecordingNotifier {
    fn notices(&self) -> Vec<ObserverNotice> {
        self.notices.lock().expect("notifier lock").clone()
    }
}

impl ObserverNotifier for RecordingNotifier {
    fn notify(&self, event: ObserverNotice) {
        self.notices.lock().expect("notifier lock").push(event);
    }
}

fn strip(id: &str, started_ms: i64, intervene_s: u32) -> FlightStrip {
    FlightStrip {
        id: format!("obs_{id}"),
        root_id: "ses_root".to_owned(),
        session_id: "ses_root".to_owned(),
        agent: "build".to_owned(),
        tool_call_id: id.to_owned(),
        tool: "shell".to_owned(),
        why: "run focused test".to_owned(),
        expect_s: 30,
        intervene_s,
        started_ms,
        retryable: true,
        ..FlightStrip::default()
    }
}

fn tree() -> Vec<TreeRow> {
    vec![TreeRow {
        sid: "ses_root".to_owned(),
        parent: None,
        agent: "build".to_owned(),
        state: "busy".to_owned(),
        depth: 0,
        idle_reason: None,
    }]
}

struct Harness {
    clock: Arc<FakeClock>,
    model: Arc<ScriptedModel>,
    executor: Arc<RecordingExecutor>,
    notifier: Arc<RecordingNotifier>,
    engine: ObserverEngine,
}

fn harness(cfg: ObserverConfig, responses: Vec<Result<String, ObserverError>>) -> Harness {
    let clock = Arc::new(FakeClock::default());
    let model = Arc::new(ScriptedModel::new(responses));
    let executor = Arc::new(RecordingExecutor::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let engine = ObserverEngine::new(
        cfg,
        clock.clone(),
        model.clone(),
        executor.clone(),
        notifier.clone(),
    );
    Harness {
        clock,
        model,
        executor,
        notifier,
        engine,
    }
}

fn nudge() -> String {
    r#"{"action":"nudge","target":"ses_root","reason":"past the deadline",
        "message":"check the focused test"}"#
        .to_owned()
}

fn retry_repair() -> String {
    r#"{"action":"repair","target":"tc_1","reason":"failed tool can be retried",
        "repair":{"kind":"retry_tool","args":{}}}"#
        .to_owned()
}

#[tokio::test]
async fn nudge_only_after_intervene_deadline() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    h.engine.set_root_context(
        "ses_root",
        RootContext {
            model: Some("anthropic/claude-sonnet-4-5".to_owned()),
            step: 3,
            ..RootContext::default()
        },
    );
    h.engine.observe_start(strip("tc_1", 0, 150));
    h.clock.set(149_999);

    assert!(h.engine.tick("ses_root", tree(), None).await.is_none());
    assert_eq!(h.model.calls(), 0, "no call before the deadline");

    h.clock.set(150_000);
    let run = h.engine.tick("ses_root", tree(), None).await;
    assert!(matches!(run, Some(ObserverRun::Verdict { .. })));
    assert_eq!(h.model.calls(), 1);
    let input = h.model.last_input().expect("rendered input");
    assert!(input.contains("\"model\":\"anthropic/claude-sonnet-4-5\""));
    assert!(input.contains("\"step\":3"));
    assert!(matches!(
        h.notifier.notices().as_slice(),
        [ObserverNotice::Nudged { .. }]
    ));
}

#[tokio::test]
async fn no_call_when_everything_is_on_schedule() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    h.engine.observe_start(strip("tc_1", 0, 600));
    h.clock.set(10_000);

    assert!(h.engine.tick("ses_root", tree(), None).await.is_none());
    assert_eq!(h.model.calls(), 0);
    assert!(h.notifier.notices().is_empty());
}

#[tokio::test]
async fn coalesces_and_rate_limits_instead_of_calling() {
    let cfg = ObserverConfig {
        min_interval_ms: 2_000,
        max_calls_per_min: 12,
        ..ObserverConfig::default()
    };
    let h = harness(cfg, vec![Ok(nudge()), Ok(nudge())]);
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.clock.set(10_000);

    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Verdict { .. })
    ));
    assert_eq!(h.model.calls(), 1);

    // A new wake (here an execution failure) inside `min_interval_ms` is
    // coalesced and served once the interval passes; the already-evaluated
    // overdue strip alone does not re-wake.
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionFailed);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Skipped {
            reason: SkipReason::Coalesced
        })
    ));
    assert_eq!(h.model.calls(), 1, "coalesced tick must not call the model");

    h.clock.set(12_000);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Verdict { .. })
    ));
    assert_eq!(h.model.calls(), 2);
}

#[tokio::test]
async fn per_minute_rate_limit_returns_skipped() {
    let cfg = ObserverConfig {
        min_interval_ms: 0,
        max_calls_per_min: 1,
        ..ObserverConfig::default()
    };
    let h = harness(cfg, vec![Ok(nudge()), Ok(nudge())]);
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.clock.set(10_000);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Verdict { .. })
    ));
    h.clock.set(11_000);
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionFailed);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Skipped {
            reason: SkipReason::RateLimited
        })
    ));
    assert_eq!(h.model.calls(), 1);
}

#[tokio::test]
async fn repair_budget_exhaustion_escalates() {
    let cfg = ObserverConfig {
        min_interval_ms: 0,
        max_repairs_per_10min: 1,
        ..ObserverConfig::default()
    };
    let h = harness(cfg, vec![Ok(retry_repair()), Ok(retry_repair())]);
    h.engine.observe_start(strip("tc_1", 0, 60));
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));

    assert!(matches!(
        h.engine.check_now("ses_root", tree(), None).await,
        Some(ObserverRun::Repair { .. })
    ));
    assert_eq!(h.executor.calls.lock().expect("executor").len(), 1);

    h.clock.set(3_000);
    let run = h
        .engine
        .check_now("ses_root", tree(), None)
        .await
        .expect("escalated run");
    assert!(matches!(run, ObserverRun::Escalated { .. }));
    assert_eq!(
        h.executor.calls.lock().expect("executor").len(),
        1,
        "no repair after escalation"
    );
    assert!(
        h.notifier
            .notices()
            .iter()
            .any(|notice| matches!(notice, ObserverNotice::Escalated { .. }))
    );
    let budget: BudgetState = h.engine.budget("ses_root");
    assert!(budget.exhausted);
    assert_eq!(budget.root_used, 1);

    h.clock.set(6_000);
    assert!(matches!(
        h.engine.check_now("ses_root", tree(), None).await,
        Some(ObserverRun::Escalated { .. })
    ));
    assert_eq!(h.executor.calls.lock().expect("executor").len(), 1);
}

#[tokio::test]
async fn retry_of_non_idempotent_tool_is_rejected() {
    let h = harness(ObserverConfig::default(), vec![Ok(retry_repair())]);
    let mut failing = strip("tc_1", 0, 60);
    failing.retryable = false;
    h.engine.observe_start(failing);
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));

    let run = h.engine.check_now("ses_root", tree(), None).await;
    match run {
        Some(ObserverRun::Rejected { reason, .. }) => {
            assert!(reason.contains("not idempotent"));
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    assert!(h.executor.calls.lock().expect("executor").is_empty());
}

#[tokio::test]
async fn notifier_receives_repaired_notice() {
    let h = harness(ObserverConfig::default(), vec![Ok(retry_repair())]);
    h.engine.observe_start(strip("tc_1", 0, 60));
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));
    h.engine.check_now("ses_root", tree(), None).await;

    let notices = h.notifier.notices();
    let repaired = notices
        .iter()
        .find_map(|notice| match notice {
            ObserverNotice::Repaired {
                target,
                kind,
                attempt,
                budget_left,
                ..
            } => Some((target.clone(), *kind, *attempt, *budget_left)),
            _ => None,
        })
        .expect("repaired notice");
    assert_eq!(repaired, ("tc_1".to_owned(), RepairKind::RetryTool, 1, 4));
}

#[tokio::test]
async fn model_failure_fails_open() {
    let h = harness(
        ObserverConfig::default(),
        vec![Err(ObserverError::Model("provider unreachable".to_owned()))],
    );
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.clock.set(10_000);

    let run = h.engine.tick("ses_root", tree(), None).await;
    assert!(matches!(run, Some(ObserverRun::Unavailable { .. })));
    assert!(h.executor.calls.lock().expect("executor").is_empty());
    assert!(
        h.notifier
            .notices()
            .iter()
            .any(|notice| matches!(notice, ObserverNotice::Unavailable { .. }))
    );
    assert_eq!(h.model.calls(), 1);
}

#[tokio::test]
async fn host_failure_flag_wakes_without_a_deadline() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    h.engine.observe_start(strip("tc_1", 0, 3_600));
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionFailed);
    h.clock.set(500);

    let run = h.engine.tick("ses_root", tree(), None).await;
    assert!(matches!(run, Some(ObserverRun::Verdict { .. })));
    assert_eq!(h.model.calls(), 1);
}

#[tokio::test]
async fn stall_wakes_after_inactivity_while_busy() {
    let cfg = ObserverConfig {
        stall_seconds: 120,
        ..ObserverConfig::default()
    };
    let h = harness(cfg, vec![Ok(nudge())]);
    h.engine.note_host_event("ses_root", HostFlag::StepEvent);
    h.clock.set(119_000);
    assert!(h.engine.tick("ses_root", tree(), None).await.is_none());
    assert_eq!(h.model.calls(), 0);

    h.clock.set(120_000);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Verdict { .. })
    ));
    assert_eq!(h.model.calls(), 1);
}

#[tokio::test]
async fn goal_change_wakes_and_done_requires_recent_evidence() {
    let done = r#"{"action":"goal_done","target":"gol_1","reason":"criteria verified",
        "goal_evidence":["tc_1"]}"#;
    let h = harness(ObserverConfig::default(), vec![Ok(done.to_owned())]);
    h.engine.observe_start(strip("tc_1", 0, 60));
    h.engine
        .observe_settle("tc_1", StripState::Ok, Some("4 tests passed"));
    let goal = GoalSnapshot {
        id: "gol_1".to_owned(),
        objective: "make tests pass".to_owned(),
        total: 1,
        ..GoalSnapshot::default()
    };
    h.clock.set(1_000);

    let run = h.engine.tick("ses_root", tree(), Some(goal)).await;
    assert!(matches!(run, Some(ObserverRun::Verdict { .. })));
    let notices = h.notifier.notices();
    let goal_notice = notices
        .iter()
        .find_map(|notice| match notice {
            ObserverNotice::Goal { status, .. } => Some(*status),
            _ => None,
        })
        .expect("goal notice");
    assert_eq!(goal_notice, oc_observer::GoalStatus::Done);

    // Once the goal is terminal (`Done`) and the tree is idle, subsequent ticks
    // must not re-wake the observer model.
    let mut done_goal = GoalSnapshot {
        id: "gol_1".to_owned(),
        objective: "make tests pass".to_owned(),
        total: 1,
        done: 1,
        status: oc_observer::GoalStatus::Done,
        rounds: 1,
        ..GoalSnapshot::default()
    };
    let mut idle_tree = tree();
    for row in &mut idle_tree {
        row.state = "idle".to_owned();
    }
    h.clock.set(60_000);
    assert!(
        h.engine
            .tick("ses_root", idle_tree, Some(done_goal.clone()))
            .await
            .is_none()
    );
    assert_eq!(h.model.calls(), 1);
    done_goal.rounds = 2;
    h.clock.set(120_000);
    assert!(
        h.engine
            .tick("ses_root", vec![], Some(done_goal))
            .await
            .is_none()
    );
    assert_eq!(h.model.calls(), 1);
}

// ---------------------------------------------------------------------------
// Regressions from session ses_e4e727e0bffdL4Id0eSw2qUdSM
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FailingExecutor {
    calls: AtomicUsize,
}

impl RepairExecutor for FailingExecutor {
    fn apply<'a>(
        &'a self,
        _verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ObserverError::Executor(
                "cancel_tool is not supported".to_owned(),
            ))
        })
    }
}

fn cancel_repair() -> String {
    r#"{"action":"repair","target":"tc_1","reason":"hung shell",
        "repair":{"kind":"cancel_tool","args":{}}}"#
        .to_owned()
}

#[tokio::test]
async fn an_overdue_strip_wakes_the_observer_once_per_backoff() {
    let cfg = ObserverConfig {
        min_interval_ms: 2_000,
        max_calls_per_min: 100,
        ..ObserverConfig::default()
    };
    let none = r#"{"action":"none","reason":"still working"}"#.to_owned();
    let h = harness(cfg, vec![Ok(none.clone()), Ok(none)]);
    // A foreground subagent: intervene after 602 s.
    h.engine.observe_start(strip("tc_1", 0, 602));
    h.clock.set(602_000);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Verdict { .. })
    ));
    // Before the fix every tick after `min_interval_ms` called the model again
    // for as long as the subagent ran (hundreds of calls).
    for second in 1..600 {
        h.clock.set(602_000 + second * 1_000);
        assert!(
            h.engine.tick("ses_root", tree(), None).await.is_none(),
            "tick {second}s after the evaluation re-woke the observer"
        );
    }
    assert_eq!(h.model.calls(), 1);
    h.clock.set(602_000 + 602_000);
    assert!(matches!(
        h.engine.tick("ses_root", tree(), None).await,
        Some(ObserverRun::Verdict { .. })
    ));
    assert_eq!(
        h.model.calls(),
        2,
        "the strip is re-checked after its backoff"
    );
}

#[tokio::test]
async fn an_escalated_root_goes_quiet_on_ticks() {
    let cfg = ObserverConfig {
        min_interval_ms: 0,
        max_repairs_per_10min: 1,
        ..ObserverConfig::default()
    };
    let h = harness(cfg, vec![Ok(retry_repair()), Ok(retry_repair())]);
    h.engine.observe_start(strip("tc_1", 0, 60));
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));
    assert!(matches!(
        h.engine.check_now("ses_root", tree(), None).await,
        Some(ObserverRun::Repair { .. })
    ));
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionFailed);
    h.clock.set(1_000);
    assert!(matches!(
        h.engine.check_now("ses_root", tree(), None).await,
        Some(ObserverRun::Escalated { .. })
    ));
    // A pending wake survived escalation before the fix, so every 1 s tick
    // produced another `Escalated` run (16 562 observation rows in the field).
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionFailed);
    for second in 2..120 {
        h.clock.set(second * 1_000);
        assert!(
            h.engine.tick("ses_root", tree(), None).await.is_none(),
            "escalated tick at {second}s must stay silent"
        );
    }
}

#[tokio::test]
async fn escalation_expires_with_the_repair_window() {
    let cfg = ObserverConfig {
        min_interval_ms: 0,
        max_repairs_per_10min: 1,
        ..ObserverConfig::default()
    };
    let window = cfg.budget_window_ms;
    let h = harness(
        cfg,
        vec![Ok(retry_repair()), Ok(retry_repair()), Ok(retry_repair())],
    );
    h.engine.observe_start(strip("tc_1", 0, 60));
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));
    h.engine.check_now("ses_root", tree(), None).await;
    h.clock.set(1_000);
    assert!(matches!(
        h.engine.check_now("ses_root", tree(), None).await,
        Some(ObserverRun::Escalated { .. })
    ));
    h.clock.set(window + 1_000);
    assert!(!h.engine.budget("ses_root").exhausted);
    assert!(matches!(
        h.engine.check_now("ses_root", tree(), None).await,
        Some(ObserverRun::Repair { .. })
    ));
}

#[tokio::test]
async fn one_exhausted_target_does_not_escalate_the_root() {
    let cfg = ObserverConfig {
        min_interval_ms: 0,
        max_repairs_per_10min: 10,
        max_repairs_per_target_10min: 1,
        ..ObserverConfig::default()
    };
    let h = harness(cfg, vec![Ok(retry_repair()), Ok(retry_repair())]);
    h.engine.observe_start(strip("tc_1", 0, 60));
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));
    h.engine.check_now("ses_root", tree(), None).await;
    h.clock.set(1_000);
    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        matches!(run, Some(ObserverRun::Rejected { .. })),
        "got {run:?}"
    );
    assert!(
        !h.notifier
            .notices()
            .iter()
            .any(|notice| matches!(notice, ObserverNotice::Escalated { .. }))
    );
}

#[tokio::test]
async fn a_failed_repair_does_not_consume_the_budget() {
    let cfg = ObserverConfig {
        min_interval_ms: 0,
        max_repairs_per_10min: 2,
        max_repairs_per_target_10min: 2,
        ..ObserverConfig::default()
    };
    let clock = Arc::new(FakeClock::default());
    let model = Arc::new(ScriptedModel::new(vec![
        Ok(cancel_repair()),
        Ok(cancel_repair()),
        Ok(cancel_repair()),
    ]));
    let executor = Arc::new(FailingExecutor::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let engine = ObserverEngine::new(cfg, clock.clone(), model, executor.clone(), notifier);
    engine.observe_start(strip("tc_1", 0, 10));
    for second in 0..3 {
        clock.set(20_000 + second * 1_000);
        let run = engine.check_now("ses_root", tree(), None).await;
        assert!(
            matches!(run, Some(ObserverRun::Unavailable { .. })),
            "got {run:?}"
        );
    }
    assert_eq!(executor.calls.load(Ordering::SeqCst), 3);
    let budget = engine.budget("ses_root");
    assert_eq!(budget.root_used, 0);
    assert!(!budget.exhausted);
}

#[tokio::test]
async fn subagent_strip_is_not_overdue_while_child_session_is_busy() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    let mut subagent_strip = strip("tc_sub", 0, 10);
    subagent_strip.tool = "subagent".to_owned();
    h.engine.observe_start(subagent_strip);

    h.clock.set(30_000);
    let busy_tree = vec![
        TreeRow {
            sid: "ses_root".to_owned(),
            parent: None,
            agent: "build".to_owned(),
            state: "busy".to_owned(),
            depth: 0,
            idle_reason: None,
        },
        TreeRow {
            sid: "ses_child".to_owned(),
            parent: Some("ses_root".to_owned()),
            agent: "general".to_owned(),
            state: "busy".to_owned(),
            depth: 1,
            idle_reason: None,
        },
    ];
    assert!(
        h.engine.tick("ses_root", busy_tree, None).await.is_none(),
        "parent subagent strip must not wake observer while child session is busy"
    );
    assert_eq!(h.model.calls(), 0);
}

#[tokio::test]
async fn cancel_tool_accepts_session_id_of_running_strip() {
    let verdict = serde_json::json!({
        "action": "repair",
        "target": "ses_root",
        "reason": "the running tool is wedged",
        "repair": { "kind": "cancel_tool" }
    })
    .to_string();
    let h = harness(ObserverConfig::default(), vec![Ok(verdict)]);
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.clock.set(10_000);
    let run = h.engine.tick("ses_root", tree(), None).await;
    assert!(
        matches!(run, Some(ObserverRun::Repair { .. })),
        "got {run:?}"
    );
}

// ---------------------------------------------------------------------------
// User stop is terminal (docs/08 §5)
// ---------------------------------------------------------------------------

fn tree_with(state: &str, idle_reason: Option<&str>) -> Vec<TreeRow> {
    vec![TreeRow {
        sid: "ses_root".to_owned(),
        parent: None,
        agent: "build".to_owned(),
        state: state.to_owned(),
        depth: 0,
        idle_reason: idle_reason.map(str::to_owned),
    }]
}

fn repost_repair(target: &str) -> String {
    format!(
        r#"{{"action":"repair","target":"{target}","reason":"the session looks stalled",
            "repair":{{"kind":"repost"}}}}"#
    )
}

#[tokio::test]
async fn user_stopped_root_does_not_wake_the_observer() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    // An overdue strip plus a pending wake: without the guard both the timer
    // tick and the forced check would call the model.
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionInterrupted);
    h.clock.set(1_000_000);
    assert_eq!(h.engine.earliest_deadline("ses_root"), Some(10_000));

    let stopped = tree_with("interrupted", Some("user"));
    assert!(
        h.engine
            .tick("ses_root", stopped.clone(), None)
            .await
            .is_none(),
        "a timer tick must not wake a user-stopped root"
    );
    assert_eq!(h.model.calls(), 0);

    assert!(
        h.engine
            .check_now("ses_root", stopped, None)
            .await
            .is_none(),
        "a forced check must not re-raise a user-stopped root"
    );
    assert_eq!(h.model.calls(), 0);
    assert!(h.executor.calls.lock().expect("executor").is_empty());
    assert!(h.notifier.notices().is_empty());
}

#[tokio::test]
async fn a_non_user_interrupt_still_wakes() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionInterrupted);
    h.clock.set(500);

    let run = h
        .engine
        .tick("ses_root", tree_with("interrupted", Some("shutdown")), None)
        .await;
    assert!(
        matches!(run, Some(ObserverRun::Verdict { .. })),
        "a shutdown interrupt must still wake, got {run:?}"
    );
    assert_eq!(h.model.calls(), 1);
}

#[tokio::test]
async fn repost_is_rejected_for_a_user_stopped_target() {
    let h = harness(
        ObserverConfig::default(),
        vec![Ok(repost_repair("ses_root"))],
    );
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.clock.set(30_000);

    let run = h
        .engine
        .tick("ses_root", tree_with("interrupted", Some("user")), None)
        .await;
    // The run-level guard fires before the verdict is requested, so the
    // rejection is observable as "no model call, no repair, no notice" rather
    // than as `ObserverRun::Rejected`.
    assert!(
        run.is_none(),
        "a repost verdict must never be requested for a stopped root, got {run:?}"
    );
    assert_eq!(h.model.calls(), 0);
    assert!(
        h.model.last_input().is_none(),
        "the repost verdict must never even be requested"
    );
    assert!(
        h.executor.calls.lock().expect("executor").is_empty(),
        "no repost may reach the executor"
    );
    assert!(h.notifier.notices().is_empty());
}

#[tokio::test]
async fn the_guard_lifts_after_a_new_execution() {
    let h = harness(ObserverConfig::default(), vec![Ok(nudge())]);
    h.engine.observe_start(strip("tc_1", 0, 10));
    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionInterruptedByUser);
    h.clock.set(30_000);
    assert!(
        h.engine
            .tick("ses_root", tree_with("interrupted", Some("user")), None)
            .await
            .is_none()
    );
    assert_eq!(h.model.calls(), 0);

    h.engine
        .note_host_event("ses_root", HostFlag::ExecutionStarted);
    let run = h.engine.tick("ses_root", tree(), None).await;
    assert!(
        matches!(run, Some(ObserverRun::Verdict { .. })),
        "a new execution lifts the guard, got {run:?}"
    );
    assert_eq!(h.model.calls(), 1);
}
