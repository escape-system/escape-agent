//! A repair is only reported as performed when the host confirms it.
//!
//! The engine's only success channel is [`ObserverNotice::Repaired`]; the host
//! (`oc-server::observer_host::BusObserverNotifier`) maps exactly that notice to
//! the `observer_repair` audit row and the `observer.repaired` event. So
//! "no `Repaired` notice" is the testable form of "no audit row and no
//! success event", and it is what these tests assert.
//!
//! Each test also asserts the *state the user sees*: the run result and the
//! repair budget, so a failure cannot hide behind a missing notice.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use oc_observer::{
    BoxFuture, Clock, FlightStrip, ObserverConfig, ObserverEngine, ObserverError, ObserverModel,
    ObserverNotice, ObserverNotifier, ObserverRun, ObserverVerdict, RepairExecutor, RepairKind,
    RepairOutcome, StripState, TreeRow,
};

#[derive(Debug, Default)]
struct FakeClock {
    wall: AtomicI64,
    mono: AtomicI64,
}

impl FakeClock {
    fn set(&self, now: i64) {
        self.wall.store(now, Ordering::SeqCst);
        self.mono.store(now, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.wall.load(Ordering::SeqCst)
    }

    fn mono_ms(&self) -> i64 {
        self.mono.load(Ordering::SeqCst)
    }
}

/// A model that replays one verdict per call.
struct ScriptedModel {
    responses: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
}

impl ObserverModel for ScriptedModel {
    fn judge<'a>(
        &'a self,
        _input: &'a str,
        _cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .responses
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| String::from(r#"{"action":"none","reason":"script exhausted"}"#));
        Box::pin(async move { Ok(next) })
    }
}

/// A host that answers exactly like the runtime does: it either confirms the
/// repair, refuses it, or reports that no answer arrived.
#[derive(Clone)]
struct AnsweringExecutor {
    answer: Result<RepairOutcome, ObserverError>,
    calls: Arc<AtomicUsize>,
}

impl AnsweringExecutor {
    fn accepted() -> Self {
        Self::new(Ok(RepairOutcome {
            applied: true,
            detail: Some("runtime applied it".to_owned()),
        }))
    }

    fn refused(reason: &str) -> Self {
        Self::new(Ok(RepairOutcome {
            applied: false,
            detail: Some(reason.to_owned()),
        }))
    }

    fn failed(reason: &str) -> Self {
        Self::new(Err(ObserverError::Executor(reason.to_owned())))
    }

    fn new(answer: Result<RepairOutcome, ObserverError>) -> Self {
        Self {
            answer,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl RepairExecutor for AnsweringExecutor {
    fn apply<'a>(
        &'a self,
        _verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }
}

/// A host that never answers at all: the reply sender was dropped, or the
/// runtime task that owned the receiver is gone.
struct MuteExecutor;

impl RepairExecutor for MuteExecutor {
    fn apply<'a>(
        &'a self,
        _verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        Box::pin(std::future::pending::<Result<RepairOutcome, ObserverError>>())
    }
}

/// A host that answers after a short delay, to prove `0` means "no deadline".
struct SlowExecutor {
    delay: std::time::Duration,
}

impl RepairExecutor for SlowExecutor {
    fn apply<'a>(
        &'a self,
        _verdict: &'a ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(RepairOutcome {
                applied: true,
                detail: Some("applied after a delay".to_owned()),
            })
        })
    }
}

#[derive(Default)]
struct RecordingNotifier {
    notices: Mutex<Vec<ObserverNotice>>,
}

impl ObserverNotifier for RecordingNotifier {
    fn notify(&self, event: ObserverNotice) {
        self.notices.lock().expect("notifier lock").push(event);
    }
}

impl RecordingNotifier {
    fn notices(&self) -> Vec<ObserverNotice> {
        self.notices.lock().expect("notifier lock").clone()
    }

    fn repaired(&self) -> Vec<ObserverNotice> {
        self.notices()
            .into_iter()
            .filter(|notice| matches!(notice, ObserverNotice::Repaired { .. }))
            .collect()
    }

    fn failures(&self) -> Vec<String> {
        self.notices()
            .into_iter()
            .filter_map(|notice| match notice {
                ObserverNotice::RepairFailed { reason, .. } => Some(reason),
                _ => None,
            })
            .collect()
    }

    fn has_inspected(&self) -> bool {
        self.notices()
            .iter()
            .any(|notice| matches!(notice, ObserverNotice::Inspected { .. }))
    }
}

fn tree() -> Vec<TreeRow> {
    vec![TreeRow {
        sid: "ses_root".to_owned(),
        parent: None,
        agent: "engineer".to_owned(),
        state: "busy".to_owned(),
        depth: 0,
        idle_reason: None,
    }]
}

fn strip(id: &str, intervene_s: u32) -> FlightStrip {
    FlightStrip {
        id: format!("obs_{id}"),
        root_id: "ses_root".to_owned(),
        session_id: "ses_root".to_owned(),
        agent: "engineer".to_owned(),
        tool_call_id: id.to_owned(),
        tool: "shell".to_owned(),
        why: "run focused test".to_owned(),
        expect_s: 30,
        intervene_s,
        started_ms: 0,
        started_mono_ms: Some(0),
        retryable: true,
        ..FlightStrip::default()
    }
}

fn cfg() -> ObserverConfig {
    ObserverConfig {
        min_interval_ms: 0,
        max_calls_per_min: 60,
        // `switch_model` is only valid when an escalation model is configured.
        escalation_model: Some("test/escalation".to_owned()),
        ..ObserverConfig::default()
    }
}

struct Harness {
    clock: Arc<FakeClock>,
    notifier: Arc<RecordingNotifier>,
    engine: ObserverEngine,
}

fn harness(cfg: ObserverConfig, verdict: &str, executor: AnsweringExecutor) -> Harness {
    harness_with(cfg, verdict, Arc::new(executor))
}

fn harness_with(cfg: ObserverConfig, verdict: &str, executor: Arc<dyn RepairExecutor>) -> Harness {
    let clock = Arc::new(FakeClock::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let model = Arc::new(ScriptedModel {
        responses: Mutex::new(VecDeque::from([verdict.to_owned()])),
        calls: AtomicUsize::new(0),
    });
    let engine = ObserverEngine::new(cfg, clock.clone(), model, executor, notifier.clone());
    Harness {
        clock,
        notifier,
        engine,
    }
}

/// The catalog-unknown escalation the field report used.
fn switch_model_unknown() -> String {
    r#"{"action":"repair","target":"ses_root",
        "reason":"the model keeps failing",
        "repair":{"kind":"switch_model","args":{"model":"ghost/missing"}}}"#
        .to_owned()
}

fn cancel_repair() -> String {
    r#"{"action":"repair","target":"tc_1",
        "reason":"the shell is wedged",
        "repair":{"kind":"cancel_tool","args":{}}}"#
        .to_owned()
}

fn retry_repair() -> String {
    r#"{"action":"repair","target":"tc_1",
        "reason":"the failed test can be retried",
        "repair":{"kind":"retry_tool","args":{}}}"#
        .to_owned()
}

fn inspect_verdict() -> String {
    r#"{"action":"inspect","target":"ses_root",
        "reason":"summarize the stalled session"}"#
        .to_owned()
}

// ---------------------------------------------------------------------------
// T1: a refused repair is not a success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t1_a_refused_switch_model_is_never_reported_as_repaired() {
    // The runtime refuses a model the catalog cannot run. Before the fix the
    // engine ignored `applied` and published `Repaired`, which the host turned
    // into an audit row plus `observer.repaired` (docs/18-status.md:403-408).
    let h = harness(
        cfg(),
        &switch_model_unknown(),
        AnsweringExecutor::refused(
            "repair model \"ghost/missing\" is not in the catalog: unknown model",
        ),
    );
    h.clock.set(10_000);

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        h.notifier.repaired().is_empty(),
        "a refused repair must not publish the success notice (audit row + observer.repaired): {:?}",
        h.notifier.notices()
    );
    let failures = h.notifier.failures();
    assert_eq!(failures.len(), 1, "one failure notice: {failures:?}");
    assert!(
        failures[0].contains("not in the catalog"),
        "the failure must carry the runtime's reason: {}",
        failures[0]
    );
    assert!(
        matches!(run, Some(ObserverRun::Rejected { .. })),
        "a refused repair is a rejection, got {run:?}"
    );
    assert_eq!(
        h.engine.budget("ses_root").root_used,
        1,
        "a refused attempt is charged, so a doomed repair escalates instead of retrying forever"
    );
}

// ---------------------------------------------------------------------------
// T2: an unanswered repair is never a success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t2_a_mute_host_times_out_as_failure_not_success() {
    // The host never resolves the receipt future (its reply sender was
    // dropped, or the runtime task that owned it is gone). The engine must
    // bound the wait and report a failure. This is the crucial property: an
    // absence of an answer can never become `observer.repaired`.
    let cfg = ObserverConfig {
        repair_ack_timeout_ms: 20,
        ..cfg()
    };
    let h = harness_with(cfg, &cancel_repair(), Arc::new(MuteExecutor));
    h.clock.set(10_000);
    h.engine.observe_start(strip("tc_1", 10));

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        h.notifier.repaired().is_empty(),
        "a repair with no receipt must not publish the success notice: {:?}",
        h.notifier.notices()
    );
    let failures = h.notifier.failures();
    assert_eq!(failures.len(), 1, "one failure notice: {failures:?}");
    assert!(
        failures[0].contains("timed out after 20 ms"),
        "the failure must name the deadline: {}",
        failures[0]
    );
    assert!(
        matches!(run, Some(ObserverRun::Unavailable { .. })),
        "an unconfirmed repair is an unavailable executor, got {run:?}"
    );
    assert_eq!(
        h.engine.budget("ses_root").root_used,
        0,
        "nothing happened, so the reservation is returned"
    );
}

#[tokio::test]
async fn t2_a_dropped_reply_sender_is_a_failure() {
    // The documented shape of the receipt is a oneshot; a dropped sender
    // surfaces as an executor error. Either way it must never be success.
    let h = harness(
        cfg(),
        &cancel_repair(),
        AnsweringExecutor::failed("repair receipt channel closed before the runtime answered"),
    );
    h.clock.set(10_000);
    h.engine.observe_start(strip("tc_1", 10));

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        h.notifier.repaired().is_empty(),
        "a dropped receipt is not a repair: {:?}",
        h.notifier.notices()
    );
    assert_eq!(h.notifier.failures().len(), 1);
    assert!(matches!(run, Some(ObserverRun::Unavailable { .. })));
    assert_eq!(h.engine.budget("ses_root").root_used, 0);
}

#[tokio::test]
async fn t2_a_zero_deadline_waits_for_the_receipt_instead_of_guessing() {
    // `repair_ack_timeout_ms = 0` opts out of the deadline (a host that always
    // answers). A slow but real receipt is still accepted.
    let cfg = ObserverConfig {
        repair_ack_timeout_ms: 0,
        ..cfg()
    };
    let h = harness_with(
        cfg,
        &switch_model_unknown(),
        Arc::new(SlowExecutor {
            delay: std::time::Duration::from_millis(30),
        }),
    );
    h.clock.set(10_000);

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert_eq!(h.notifier.repaired().len(), 1, "the receipt was accepted");
    assert!(matches!(run, Some(ObserverRun::Repair { .. })));
}

// ---------------------------------------------------------------------------
// T3: a confirmed repair is unchanged (no regression)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t3_a_confirmed_repair_still_writes_row_and_publishes_success() {
    let h = harness(
        cfg(),
        &switch_model_unknown(),
        AnsweringExecutor::accepted(),
    );
    h.clock.set(10_000);

    let run = h.engine.check_now("ses_root", tree(), None).await;
    let repaired = h.notifier.repaired();
    assert_eq!(repaired.len(), 1, "the success notice: {repaired:?}");
    assert!(matches!(
        repaired[0],
        ObserverNotice::Repaired {
            kind: RepairKind::SwitchModel,
            attempt: 1,
            budget_left: 4,
            ..
        }
    ));
    match run {
        Some(ObserverRun::Repair { outcome, .. }) => assert!(outcome.applied),
        other => panic!("expected a Repair run, got {other:?}"),
    }
    assert!(h.notifier.failures().is_empty());
    assert_eq!(h.engine.budget("ses_root").root_used, 1);
}

// ---------------------------------------------------------------------------
// T4: the no-op kinds must not claim success
// ---------------------------------------------------------------------------

#[tokio::test]
async fn t4_a_retry_tool_the_runtime_cannot_apply_is_a_failure() {
    // `retry_tool` has no runtime command; the host reports an executor error
    // instead of pretending it ran (oc-runtime observer/repair.rs:126-128).
    let h = harness(
        cfg(),
        &retry_repair(),
        AnsweringExecutor::failed("retry_tool is not wired into the runtime yet"),
    );
    h.clock.set(10_000);
    h.engine.observe_start(strip("tc_1", 60));
    h.engine
        .observe_settle("tc_1", StripState::Error, Some("exit 1"));

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        h.notifier.repaired().is_empty(),
        "an unimplemented repair kind must not report success: {:?}",
        h.notifier.notices()
    );
    assert_eq!(h.notifier.failures().len(), 1);
    assert!(matches!(run, Some(ObserverRun::Unavailable { .. })));
    assert_eq!(
        h.engine.budget("ses_root").root_used,
        0,
        "a repair that never dispatched is not charged"
    );
}

#[tokio::test]
async fn t4_a_cancel_tool_that_cancelled_nothing_is_a_failure() {
    // The runtime cancels zero tool calls (no matching running tool) and used
    // to return success anyway; the honest answer is a refusal.
    let h = harness(
        cfg(),
        &cancel_repair(),
        AnsweringExecutor::refused("cancel_tool found no running tool call"),
    );
    h.clock.set(10_000);
    h.engine.observe_start(strip("tc_1", 10));

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        h.notifier.repaired().is_empty(),
        "cancel_tool must not report success when nothing was cancelled: {:?}",
        h.notifier.notices()
    );
    assert_eq!(h.notifier.failures().len(), 1);
    assert!(matches!(run, Some(ObserverRun::Rejected { .. })));
}

// ---------------------------------------------------------------------------
// A refused compaction does not hold the cooldown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_compaction_does_not_hold_the_compact_cooldown() {
    // The attempt is charged (so a doomed repair escalates instead of being
    // re-proposed forever), but because nothing was compacted the next genuine
    // compaction is not blocked for `compact_cooldown_ms`.
    let compact = r#"{"action":"repair","target":"ses_root","reason":"context is nearly full",
            "repair":{"kind":"compact_context","args":{}}}"#
        .to_owned();
    let clock = Arc::new(FakeClock::default());
    let notifier = Arc::new(RecordingNotifier::default());
    let model = Arc::new(ScriptedModel {
        responses: Mutex::new(VecDeque::from([compact.clone(), compact])),
        calls: AtomicUsize::new(0),
    });
    let engine = ObserverEngine::new(
        ObserverConfig {
            min_interval_ms: 0,
            compact_cooldown_ms: 600_000,
            ..cfg()
        },
        clock.clone(),
        model,
        Arc::new(AnsweringExecutor::refused("no compaction channel")),
        notifier.clone(),
    );

    clock.set(10_000);
    let first = engine.check_now("ses_root", tree(), None).await;
    assert!(
        matches!(first, Some(ObserverRun::Rejected { .. })),
        "the refusal is a rejection, got {first:?}"
    );
    assert_eq!(notifier.repaired().len(), 0, "no success row or event");
    assert_eq!(
        engine.budget("ses_root").root_used,
        1,
        "the attempt is charged"
    );

    // One second later: with a stale cooldown this would be rejected as
    // "compact_context is cooling down for this target".
    clock.set(11_000);
    let second = engine.check_now("ses_root", tree(), None).await;
    assert!(
        !matches!(
            &second,
            Some(ObserverRun::Rejected { reason, .. }) if reason.contains("cooling down")
        ),
        "a refused compaction left a cooldown behind: {second:?}"
    );
}

#[tokio::test]
async fn t4_an_inspect_never_produces_a_repair_success() {
    let h = harness(cfg(), &inspect_verdict(), AnsweringExecutor::accepted());
    h.clock.set(10_000);

    let run = h.engine.check_now("ses_root", tree(), None).await;
    assert!(
        h.notifier.repaired().is_empty(),
        "inspect must not write a repair row or publish observer.repaired: {:?}",
        h.notifier.notices()
    );
    assert!(h.notifier.has_inspected(), "the inspect notice is expected");
    assert!(matches!(run, Some(ObserverRun::Verdict { .. })));
}

// ---------------------------------------------------------------------------
// The receipt contract is documented on the trait the host implements
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_receipt_detail_reaches_the_host_on_success() {
    // `RepairOutcome.detail` is the host's own wording of what it did; the
    // engine must surface the outcome unchanged on a confirmed repair.
    let h = harness(
        cfg(),
        &switch_model_unknown(),
        AnsweringExecutor::accepted(),
    );
    h.clock.set(10_000);
    let run = h.engine.check_now("ses_root", tree(), None).await;
    match run {
        Some(ObserverRun::Repair { outcome, .. }) => {
            assert_eq!(outcome.detail.as_deref(), Some("runtime applied it"));
        }
        other => panic!("expected a Repair run, got {other:?}"),
    }
}
