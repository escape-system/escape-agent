//! Scheduler behaviour: serialization, priority, cancellation, wait and
//! shutdown. Every runner is fake and every wait has a deadline.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use oc_lanes::{
    BoxFuture, CancellationTokens, Clock, DEFAULT_CONCURRENCY, LaneConfig, LaneError, LaneEvents,
    LaneRunner, LaneService, NewTicket, RunnerResult, Ticket, TicketState,
};
use tokio::sync::{Semaphore, mpsc};

const LANE: &str = "cargo:test";

#[derive(Debug, Default)]
struct TestClock(AtomicI64);

impl TestClock {
    fn new(now: i64) -> Self {
        Self(AtomicI64::new(now))
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    /// The same counter drives both clocks: the lanes service never reads the
    /// monotonic clock, but a fake that inherits the default would silently
    /// follow real time and make any future deadline assertion flaky.
    fn mono_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Default)]
struct RecordingEvents {
    seen: StdMutex<Vec<Ticket>>,
}

impl LaneEvents for RecordingEvents {
    fn changed(&self, ticket: &Ticket) {
        self.seen.lock().expect("events").push(ticket.clone());
    }
}

struct ControlledRunner {
    active: AtomicUsize,
    max_active: AtomicUsize,
    release: Arc<Semaphore>,
    started: mpsc::UnboundedSender<Ticket>,
    finished: mpsc::UnboundedSender<String>,
    tokens: StdMutex<Option<CancellationTokens>>,
}

impl ControlledRunner {
    fn new() -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<Ticket>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (started, started_rx) = mpsc::unbounded_channel();
        let (finished, finished_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                release: Arc::new(Semaphore::new(0)),
                started,
                finished,
                tokens: StdMutex::new(None),
            }),
            started_rx,
            finished_rx,
        )
    }

    fn attach(&self, service: &LaneService) {
        *self.tokens.lock().expect("tokens") = Some(service.tokens());
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

impl LaneRunner for ControlledRunner {
    fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        let token = self
            .tokens
            .lock()
            .expect("tokens")
            .as_ref()
            .and_then(|tokens| tokens.get(&ticket.id));
        Box::pin(async move {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let _ = self.started.send(ticket.clone());
            let release = self.release.clone();
            let released = async move {
                if let Ok(permit) = release.acquire_owned().await {
                    permit.forget();
                }
            };
            if let Some(token) = token {
                tokio::select! {
                    () = released => {}
                    () = token.cancelled() => {}
                }
            } else {
                released.await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            let _ = self.finished.send(ticket.id.clone());
            RunnerResult::success()
        })
    }
}

struct ImmediateRunner {
    result: RunnerResult,
}

impl LaneRunner for ImmediateRunner {
    fn run<'a>(&'a self, _ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        let result = self.result.clone();
        Box::pin(async move { result })
    }
}

struct PanicRunner;

impl LaneRunner for PanicRunner {
    fn run<'a>(&'a self, _ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        Box::pin(async { panic!("runner exploded") })
    }
}

/// Blocks until released, then reports a captured-output file. Used to observe
/// a ticket while it is running and to race a late result against cancel.
struct OutputRunner {
    path: PathBuf,
    release: Arc<Semaphore>,
    started: mpsc::UnboundedSender<Ticket>,
    finished: mpsc::UnboundedSender<String>,
}

impl OutputRunner {
    fn new(
        path: PathBuf,
    ) -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<Ticket>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (started, started_rx) = mpsc::unbounded_channel();
        let (finished, finished_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                path,
                release: Arc::new(Semaphore::new(0)),
                started,
                finished,
            }),
            started_rx,
            finished_rx,
        )
    }

    fn release_one(&self) {
        self.release.add_permits(1);
    }
}

impl LaneRunner for OutputRunner {
    fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        let path = self.path.clone();
        Box::pin(async move {
            let _ = self.started.send(ticket.clone());
            if let Ok(permit) = self.release.clone().acquire_owned().await {
                permit.forget();
            }
            let _ = self.finished.send(ticket.id.clone());
            RunnerResult::success().with_output_path(path)
        })
    }
}

fn build(
    runner: Arc<dyn LaneRunner>,
    config: LaneConfig,
) -> (LaneService, Arc<RecordingEvents>, Arc<TestClock>) {
    let events = Arc::new(RecordingEvents::default());
    let clock = Arc::new(TestClock::new(1_000));
    let service = LaneService::new(config, runner, events.clone(), clock.clone());
    (service, events, clock)
}

fn ticket(command: &str, priority: i32) -> NewTicket {
    NewTicket {
        command: command.to_owned(),
        cwd: PathBuf::from("."),
        priority,
        owner_session: Some("ses_test".to_owned()),
        label: None,
        tree_rev: None,
    }
}

async fn enqueue(service: &LaneService, command: &str, priority: i32) -> String {
    service
        .enqueue(LANE.to_owned(), ticket(command, priority))
        .await
        .expect("enqueue")
}

async fn next_started(receiver: &mut mpsc::UnboundedReceiver<Ticket>) -> Ticket {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("start deadline")
        .expect("started channel")
}

async fn next_finished(receiver: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .expect("finish deadline")
        .expect("finished channel")
}

async fn wait_ok(service: &LaneService, id: &str) -> Ticket {
    let ticket = service
        .wait(id, Duration::from_secs(2))
        .await
        .expect("ticket finished");
    assert_eq!(ticket.state, TicketState::Succeeded { exit: 0 }, "{id}");
    ticket
}

fn recorded_states(events: &RecordingEvents, id: &str) -> Vec<TicketState> {
    events
        .seen
        .lock()
        .expect("events")
        .iter()
        .filter(|ticket| ticket.id == id)
        .map(|ticket| ticket.state.clone())
        .collect()
}

async fn terminal_event(events: &RecordingEvents, id: &str) -> Ticket {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let found = events
            .seen
            .lock()
            .expect("events")
            .iter()
            .find(|ticket| ticket.id == id && ticket.state.is_terminal())
            .cloned();
        if let Some(ticket) = found {
            return ticket;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no terminal event for {id}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn concurrency_one_serializes() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let first = enqueue(&service, "one", 0).await;
    let second = enqueue(&service, "two", 0).await;
    let running = next_started(&mut started).await;
    assert_eq!(running.id, first);
    assert!(started.try_recv().is_err(), "second ticket must wait");

    runner.release_one();
    let running = next_started(&mut started).await;
    assert_eq!(running.id, second);
    runner.release_one();

    wait_ok(&service, &first).await;
    wait_ok(&service, &second).await;
    assert_eq!(runner.max_active(), 1);
    assert_eq!(runner.active(), 0);
    service.shutdown().await;
}

#[tokio::test]
async fn higher_priority_jumps_the_queue() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let base = enqueue(&service, "base", 0).await;
    assert_eq!(next_started(&mut started).await.id, base);
    let low = enqueue(&service, "low", 1).await;
    let high = enqueue(&service, "high", 10).await;
    let middle = enqueue(&service, "middle", 5).await;
    assert!(started.try_recv().is_err());

    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, high);
    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, middle);
    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, low);
    runner.release_one();

    for id in [&base, &low, &high, &middle] {
        wait_ok(&service, id).await;
    }
    assert_eq!(runner.max_active(), 1);
    service.shutdown().await;
}

#[tokio::test]
async fn equal_priority_is_fifo() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let base = enqueue(&service, "base", 0).await;
    assert_eq!(next_started(&mut started).await.id, base);
    let first = enqueue(&service, "first", 7).await;
    let second = enqueue(&service, "second", 7).await;
    let third = enqueue(&service, "third", 7).await;

    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, first);
    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, second);
    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, third);
    runner.release_one();

    for id in [&base, &first, &second, &third] {
        wait_ok(&service, id).await;
    }
    service.shutdown().await;
}

#[tokio::test]
async fn max_queue_is_enforced() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let mut config = LaneConfig::default().with_lane(LANE, 1);
    config.max_queue = 1;
    let (service, _events, _clock) = build(runner.clone(), config);
    runner.attach(&service);

    let running = enqueue(&service, "running", 0).await;
    assert_eq!(next_started(&mut started).await.id, running);
    let queued = enqueue(&service, "queued", 0).await;
    let error = service
        .enqueue(LANE.to_owned(), ticket("overflow", 0))
        .await
        .expect_err("queue must reject a third ticket");
    assert_eq!(
        error,
        LaneError::QueueFull {
            lane: LANE.to_owned(),
            max: 1,
        }
    );

    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, queued);
    runner.release_one();
    wait_ok(&service, &running).await;
    wait_ok(&service, &queued).await;
    service.shutdown().await;
}

#[tokio::test]
async fn cancel_queued_removes_it() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let running = enqueue(&service, "running", 0).await;
    assert_eq!(next_started(&mut started).await.id, running);
    let queued = enqueue(&service, "queued", 0).await;

    assert!(service.cancel(&queued).await.expect("cancel queued"));
    assert_eq!(
        service.ticket(&queued).await.expect("ticket").state,
        TicketState::Cancelled
    );
    assert_eq!(
        service
            .wait(&queued, Duration::ZERO)
            .await
            .expect("terminal")
            .state,
        TicketState::Cancelled
    );
    assert!(!service.cancel(&queued).await.expect("idempotent"));
    assert!(matches!(
        service.cancel("missing").await,
        Err(LaneError::UnknownTicket(id)) if id == "missing"
    ));

    runner.release_one();
    wait_ok(&service, &running).await;
    assert!(
        started.try_recv().is_err(),
        "a cancelled ticket must never start"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn cancel_running_signals_the_token() {
    let (runner, mut started, mut finished) = ControlledRunner::new();
    let (service, events, _clock) = build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let id = enqueue(&service, "long", 0).await;
    assert_eq!(next_started(&mut started).await.id, id);

    assert!(service.cancel(&id).await.expect("cancel running"));
    assert_eq!(
        service
            .wait(&id, Duration::from_secs(2))
            .await
            .expect("terminal")
            .state,
        TicketState::Cancelled
    );
    assert_eq!(next_finished(&mut finished).await, id);
    assert_eq!(
        service.ticket(&id).await.expect("ticket").state,
        TicketState::Cancelled,
        "a late runner result must not overwrite cancellation"
    );
    let cancelled_events = events
        .seen
        .lock()
        .expect("events")
        .iter()
        .filter(|ticket| ticket.id == id && ticket.state == TicketState::Cancelled)
        .count();
    assert_eq!(cancelled_events, 1, "cancelled must be emitted once");
    service.shutdown().await;
}

#[tokio::test]
async fn wait_times_out_without_polling() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let running = enqueue(&service, "running", 0).await;
    assert_eq!(next_started(&mut started).await.id, running);
    let queued = enqueue(&service, "queued", 0).await;
    let error = service
        .wait(&queued, Duration::from_millis(20))
        .await
        .expect_err("queued ticket cannot finish");
    assert!(matches!(error, LaneError::Timeout { .. }));

    runner.release_one();
    wait_ok(&service, &running).await;
    runner.release_one();
    wait_ok(&service, &queued).await;
    service.shutdown().await;
}

#[tokio::test]
async fn status_counts_running_and_queued() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 2));
    runner.attach(&service);

    let first = enqueue(&service, "first", 0).await;
    let second = enqueue(&service, "second", 0).await;
    let third = enqueue(&service, "third", 0).await;
    assert_eq!(next_started(&mut started).await.id, first);
    assert_eq!(next_started(&mut started).await.id, second);

    let status = service.status(Some(LANE)).await;
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].concurrency, 2);
    assert_eq!(status[0].running, 2);
    assert_eq!(status[0].queued, 1);

    let all = service.status(None).await;
    assert!(all.iter().any(|lane| lane.lane == LANE));
    let unknown = service.status(Some("network")).await;
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0].concurrency, 4);
    assert_eq!(unknown[0].running, 0);
    assert_eq!(unknown[0].queued, 0);
    assert_eq!(
        service.status(Some("no-such-lane")).await[0].concurrency,
        DEFAULT_CONCURRENCY
    );

    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, third);
    runner.release_one();
    runner.release_one();
    for id in [&first, &second, &third] {
        wait_ok(&service, id).await;
    }
    let drained = service.status(Some(LANE)).await;
    assert_eq!(drained[0].running, 0);
    assert_eq!(drained[0].queued, 0);
    service.shutdown().await;
}

#[tokio::test]
async fn reorder_moves_a_queued_ticket() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let base = enqueue(&service, "base", 0).await;
    assert_eq!(next_started(&mut started).await.id, base);
    let low = enqueue(&service, "low", 1).await;
    let high = enqueue(&service, "high", 10).await;

    service.reorder(&low, 20).await.expect("reorder queued");
    assert_eq!(service.ticket(&low).await.expect("ticket").priority, 20);
    assert!(matches!(
        service.reorder(&base, 5).await,
        Err(LaneError::NotQueued { .. })
    ));
    assert!(matches!(
        service.reorder("missing", 5).await,
        Err(LaneError::UnknownTicket(_))
    ));

    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, low);
    runner.release_one();
    assert_eq!(next_started(&mut started).await.id, high);
    runner.release_one();

    for id in [&base, &low, &high] {
        wait_ok(&service, id).await;
    }
    service.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_queued_and_running() {
    let (runner, mut started, mut finished) = ControlledRunner::new();
    let (service, _events, _clock) =
        build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let running = enqueue(&service, "running", 0).await;
    assert_eq!(next_started(&mut started).await.id, running);
    let queued = enqueue(&service, "queued", 0).await;

    service.shutdown().await;
    assert_eq!(
        service
            .wait(&queued, Duration::ZERO)
            .await
            .expect("queued terminal")
            .state,
        TicketState::Cancelled
    );
    assert_eq!(
        service
            .wait(&running, Duration::from_secs(2))
            .await
            .expect("running terminal")
            .state,
        TicketState::Cancelled
    );
    next_finished(&mut finished).await;
    assert!(matches!(
        service.enqueue(LANE.to_owned(), ticket("late", 0)).await,
        Err(LaneError::Shutdown)
    ));
    assert_eq!(service.status(Some(LANE)).await[0].queued, 0);
}

#[tokio::test]
async fn non_zero_exit_is_failed() {
    let runner = Arc::new(ImmediateRunner {
        result: RunnerResult::failure(2, "tests failed"),
    });
    let (service, _events, _clock) = build(runner, LaneConfig::default().with_lane(LANE, 1));
    let id = enqueue(&service, "cargo test", 0).await;
    let ticket = service
        .wait(&id, Duration::from_secs(2))
        .await
        .expect("terminal");
    assert_eq!(
        ticket.state,
        TicketState::Failed {
            exit: 2,
            message: "tests failed".to_owned(),
        }
    );
    service.shutdown().await;
}

#[tokio::test]
async fn panicking_runner_is_failed() {
    let (service, _events, _clock) = build(
        Arc::new(PanicRunner),
        LaneConfig::default().with_lane(LANE, 1),
    );
    let id = enqueue(&service, "cargo test", 0).await;
    let ticket = service
        .wait(&id, Duration::from_secs(2))
        .await
        .expect("terminal");
    match ticket.state {
        TicketState::Failed { exit, message } => {
            assert_eq!(exit, -1);
            assert_eq!(message, "lane runner panicked");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    service.shutdown().await;
}

#[tokio::test]
async fn waiter_observes_runner_output_path() {
    let path = PathBuf::from("/tmp/lane-cargo-test.log");
    let runner = Arc::new(ImmediateRunner {
        result: RunnerResult::success().with_output_path(path.clone()),
    });
    let (service, events, _clock) = build(runner, LaneConfig::default().with_lane(LANE, 1));

    let id = enqueue(&service, "cargo test", 0).await;
    let ticket = wait_ok(&service, &id).await;
    assert_eq!(ticket.output_path, Some(path.clone()));

    let running = events
        .seen
        .lock()
        .expect("events")
        .iter()
        .find(|ticket| ticket.id == id && ticket.state == TicketState::Running)
        .cloned()
        .expect("running event");
    assert_eq!(
        running.output_path, None,
        "a running ticket has no output path yet"
    );

    let terminal = terminal_event(&events, &id).await;
    assert_eq!(terminal.state, TicketState::Succeeded { exit: 0 });
    assert_eq!(
        terminal.output_path,
        Some(path),
        "the terminal event carries the path"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn failed_ticket_keeps_runner_output_path() {
    let path = PathBuf::from("/tmp/lane-cargo-test.failed.log");
    let runner = Arc::new(ImmediateRunner {
        result: RunnerResult::failure(2, "tests failed").with_output_path(path.clone()),
    });
    let (service, _events, _clock) = build(runner, LaneConfig::default().with_lane(LANE, 1));

    let id = enqueue(&service, "cargo test", 0).await;
    let ticket = service
        .wait(&id, Duration::from_secs(2))
        .await
        .expect("terminal");
    assert_eq!(
        ticket.state,
        TicketState::Failed {
            exit: 2,
            message: "tests failed".to_owned(),
        }
    );
    assert_eq!(ticket.output_path, Some(path));
    service.shutdown().await;
}

#[tokio::test]
async fn ticket_moves_queued_running_succeeded() {
    let (runner, mut started, _finished) = ControlledRunner::new();
    let (service, events, _clock) = build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));
    runner.attach(&service);

    let id = enqueue(&service, "cargo test", 0).await;
    let running = next_started(&mut started).await;
    assert_eq!(running.id, id);
    assert_eq!(running.state, TicketState::Running);
    assert_eq!(running.output_path, None);

    runner.release_one();
    let ticket = wait_ok(&service, &id).await;
    assert_eq!(ticket.output_path, None);
    assert_eq!(terminal_event(&events, &id).await.state, ticket.state);
    assert_eq!(
        recorded_states(&events, &id),
        vec![
            TicketState::Queued,
            TicketState::Running,
            TicketState::Succeeded { exit: 0 },
        ],
        "a ticket moves queued -> running -> succeeded exactly once each"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn cancellation_does_not_report_a_late_output_path() {
    let path = PathBuf::from("/tmp/lane-cancelled.log");
    let (runner, mut started, mut finished) = OutputRunner::new(path);
    let (service, events, _clock) = build(runner.clone(), LaneConfig::default().with_lane(LANE, 1));

    let id = enqueue(&service, "cargo test", 0).await;
    assert_eq!(next_started(&mut started).await.id, id);

    assert!(service.cancel(&id).await.expect("cancel running"));
    let cancelled = service
        .wait(&id, Duration::from_secs(2))
        .await
        .expect("terminal");
    assert_eq!(cancelled.state, TicketState::Cancelled);
    assert_eq!(cancelled.output_path, None);

    // The runner keeps its slot until it returns; its late result must not
    // attach an output path to the already terminal ticket.
    runner.release_one();
    assert_eq!(next_finished(&mut finished).await, id);
    let ticket = service.ticket(&id).await.expect("ticket");
    assert_eq!(ticket.state, TicketState::Cancelled);
    assert_eq!(ticket.output_path, None);
    assert!(
        events
            .seen
            .lock()
            .expect("events")
            .iter()
            .filter(|ticket| ticket.id == id)
            .all(|ticket| ticket.output_path.is_none()),
        "cancellation wins over the runner's output path"
    );
    service.shutdown().await;
}
