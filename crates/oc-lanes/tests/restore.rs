//! Boot restore and enqueue coalescing.
//!
//! Every source is a fake `LaneTicketSource`, every runner is deterministic and
//! every wait has a deadline, so the tests are offline and reproducible.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use oc_lanes::{
    BoxFuture, Clock, LaneConfig, LaneEvents, LaneRunner, LaneService, LaneTicketSource, NewTicket,
    RestoreError, RunnerResult, Ticket, TicketRecord, TicketState,
};
use tokio::sync::Semaphore;

const LANE: &str = "cargo:test";

#[derive(Debug, Default)]
struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Default)]
struct NoEvents;

impl LaneEvents for NoEvents {
    fn changed(&self, _ticket: &Ticket) {}
}

/// Records every published ticket so a test can assert that a restore does not
/// re-publish history.
#[derive(Debug, Default)]
struct RecordingEvents {
    published: StdMutex<Vec<Ticket>>,
}

impl LaneEvents for RecordingEvents {
    fn changed(&self, ticket: &Ticket) {
        self.published
            .lock()
            .expect("published")
            .push(ticket.clone());
    }
}

impl RecordingEvents {
    fn ids(&self) -> Vec<String> {
        self.published
            .lock()
            .expect("published")
            .iter()
            .map(|ticket| ticket.id.clone())
            .collect()
    }
}

/// Persisted-ticket source; `load` returns a clone so a test can compare the
/// records it handed in with the ids the service kept.
struct FakeSource {
    records: Vec<TicketRecord>,
}

impl FakeSource {
    fn new(records: Vec<TicketRecord>) -> Arc<Self> {
        Arc::new(Self { records })
    }
}

impl LaneTicketSource for FakeSource {
    fn load(&self) -> BoxFuture<'_, Result<Vec<TicketRecord>, RestoreError>> {
        let records = self.records.clone();
        Box::pin(async move { Ok(records) })
    }
}

/// Records the command of every execution and succeeds immediately.
#[derive(Debug, Default)]
struct RecordingRunner {
    commands: StdMutex<Vec<String>>,
}

impl LaneRunner for RecordingRunner {
    fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        self.commands
            .lock()
            .expect("commands")
            .push(ticket.command.clone());
        Box::pin(async { RunnerResult::success() })
    }
}

/// Blocks every execution until a permit is released; records start order.
struct BlockingRunner {
    release: Arc<Semaphore>,
    started: StdMutex<Vec<String>>,
}

impl BlockingRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            release: Arc::new(Semaphore::new(0)),
            started: StdMutex::new(Vec::new()),
        })
    }

    fn release(&self, permits: usize) {
        self.release.add_permits(permits);
    }

    fn started(&self) -> usize {
        self.started.lock().expect("started").len()
    }
}

impl LaneRunner for BlockingRunner {
    fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        Box::pin(async move {
            self.started
                .lock()
                .expect("started")
                .push(ticket.command.clone());
            if let Ok(permit) = self.release.clone().acquire_owned().await {
                permit.forget();
            }
            RunnerResult::success()
        })
    }
}

fn record(id: &str, command: &str, state: TicketState, queued_ms: i64) -> TicketRecord {
    TicketRecord {
        id: id.to_owned(),
        lane: LANE.to_owned(),
        command: command.to_owned(),
        cwd: PathBuf::from("."),
        priority: 0,
        owner_session: Some("ses_restore".to_owned()),
        state,
        queued_ms,
        started_ms: None,
        finished_ms: None,
        output_path: None,
    }
}

fn config() -> LaneConfig {
    LaneConfig::default().with_lane(LANE, 1)
}

async fn wait_ok(service: &LaneService, id: &str) -> Ticket {
    let ticket = service
        .wait(id, Duration::from_secs(2))
        .await
        .expect("ticket finished");
    assert_eq!(ticket.state, TicketState::Succeeded { exit: 0 }, "{id}");
    ticket
}

fn ticket(command: &str, tree_rev: Option<&str>, owner: &str) -> NewTicket {
    NewTicket {
        command: command.to_owned(),
        cwd: PathBuf::from("."),
        priority: 0,
        owner_session: Some(owner.to_owned()),
        label: None,
        tree_rev: tree_rev.map(str::to_owned),
    }
}

/// Wait until `count` executions have started.
async fn wait_started(runner: &BlockingRunner, count: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while runner.started() < count {
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} of {count} tickets started",
            runner.started()
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn restore_replays_queued_and_fails_running() {
    // Deliberately out of order: replay sorts by queued_ms.
    let records = vec![
        record("tkt_2", "two", TicketState::Queued, 200),
        record("tkt_4", "four", TicketState::Succeeded { exit: 0 }, 400),
        record("tkt_1", "one", TicketState::Queued, 100),
        TicketRecord {
            started_ms: Some(350),
            ..record("tkt_3", "three", TicketState::Running, 300)
        },
    ];
    let persisted_ids: Vec<String> = records.iter().map(|record| record.id.clone()).collect();
    let source = FakeSource::new(records);
    let runner = Arc::new(RecordingRunner::default());
    let service = LaneService::restore(
        config(),
        runner.clone(),
        Arc::new(NoEvents),
        Arc::new(TestClock::default()),
        source,
    )
    .await
    .expect("restore");

    // (c) a running row belongs to a process that is gone.
    let interrupted = service.ticket("tkt_3").await.expect("tkt_3");
    match &interrupted.state {
        TicketState::Failed { exit, message } => {
            assert_eq!(*exit, -1);
            assert!(message.contains("restart"), "{message}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // (d) terminal history is untouched.
    assert_eq!(
        service.ticket("tkt_4").await.expect("tkt_4").state,
        TicketState::Succeeded { exit: 0 }
    );

    // (b) queued rows run in their original enqueue order.
    wait_ok(&service, "tkt_1").await;
    wait_ok(&service, "tkt_2").await;
    assert_eq!(
        runner.commands.lock().expect("commands").clone(),
        vec!["one".to_owned(), "two".to_owned()]
    );

    // (a) the next id is above every persisted suffix: max was 4.
    let next = service
        .enqueue_detailed(LANE.to_owned(), ticket("five", None, "ses_restore"))
        .await
        .expect("enqueue after restore");
    assert_eq!(next.id, "tkt_5");
    assert!(!next.subscribed);
    wait_ok(&service, "tkt_5").await;

    // (e) every persisted row still exists under its own id and none was
    // reissued: the old ids are distinct from the new one and all preserved.
    for id in &persisted_ids {
        let stored = service.ticket(id).await.expect("persisted row survives");
        assert_eq!(&stored.id, id);
    }
    let mut ids = persisted_ids.clone();
    ids.push(next.id.clone());
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), persisted_ids.len() + 1, "no id was reissued");

    service.shutdown().await;
}

#[tokio::test]
async fn restore_on_an_empty_store_is_a_fresh_service() {
    let service = LaneService::restore(
        config(),
        Arc::new(RecordingRunner::default()),
        Arc::new(NoEvents),
        Arc::new(TestClock::default()),
        FakeSource::new(Vec::new()),
    )
    .await
    .expect("restore on an empty store");

    let first = service
        .enqueue_detailed(LANE.to_owned(), ticket("cargo test", None, "ses_a"))
        .await
        .expect("enqueue");
    assert_eq!(first.id, "tkt_1");
    wait_ok(&service, "tkt_1").await;
    service.shutdown().await;
}

#[tokio::test]
async fn enqueue_coalesces_only_with_a_tree_rev() {
    let runner = BlockingRunner::new();
    let service = LaneService::new(
        config(),
        runner.clone(),
        Arc::new(NoEvents),
        Arc::new(TestClock::default()),
    );

    // First submission owns the ticket; it is not its own subscriber.
    let first = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo test -p oc-x", Some("rev1"), "ses_a"),
        )
        .await
        .expect("first");
    assert_eq!(first.id, "tkt_1");
    assert!(!first.subscribed);
    wait_started(&runner, 1).await;
    assert_eq!(
        service.ticket("tkt_1").await.expect("tkt_1").subscribers,
        Vec::<String>::new()
    );

    // Same key, different spacing and a trailing `;`: still one ticket.
    let second = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo  test -p oc-x ;", Some("rev1"), "ses_b"),
        )
        .await
        .expect("second");
    assert_eq!(second.id, "tkt_1");
    assert!(second.subscribed);
    assert_eq!(
        service.ticket("tkt_1").await.expect("tkt_1").subscribers,
        vec!["ses_b".to_owned()]
    );

    // Re-subscribing the same session does not duplicate it.
    let again = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo test -p oc-x", Some("rev1"), "ses_b"),
        )
        .await
        .expect("third");
    assert_eq!(again.id, "tkt_1");
    assert_eq!(
        service.ticket("tkt_1").await.expect("tkt_1").subscribers,
        vec!["ses_b".to_owned()]
    );
    assert_eq!(runner.started(), 1, "the runner must execute once");

    // A different tree state is different work.
    let other_rev = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo test -p oc-x", Some("rev2"), "ses_c"),
        )
        .await
        .expect("other revision");
    assert_eq!(other_rev.id, "tkt_2");
    assert!(!other_rev.subscribed);

    // No tree revision: never coalesce.
    let no_rev = service
        .enqueue_detailed(LANE.to_owned(), ticket("cargo test -p oc-x", None, "ses_d"))
        .await
        .expect("no revision");
    assert_eq!(no_rev.id, "tkt_3");
    assert!(!no_rev.subscribed);

    runner.release(3);
    wait_ok(&service, "tkt_1").await;
    wait_ok(&service, "tkt_2").await;
    wait_ok(&service, "tkt_3").await;

    // A finished ticket is never reused, even with the identical key.
    let after_finish = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo test -p oc-x", Some("rev1"), "ses_a"),
        )
        .await
        .expect("after finish");
    assert_eq!(after_finish.id, "tkt_4");
    assert!(!after_finish.subscribed);
    runner.release(1);
    wait_ok(&service, "tkt_4").await;

    service.shutdown().await;
}

/// A completed row keeps its `finishedMs` and `outputPath` in memory, is never
/// re-published by a boot, and a second restore of the same source is
/// idempotent.
#[tokio::test]
async fn restore_never_republishes_terminal_history() {
    let completed = TicketRecord {
        started_ms: Some(150),
        finished_ms: Some(400),
        output_path: Some(PathBuf::from("/tmp/lane-history.log")),
        ..record(
            "tkt_2",
            "cargo build",
            TicketState::Succeeded { exit: 0 },
            100,
        )
    };
    let events = Arc::new(RecordingEvents::default());
    let build = || {
        LaneService::restore(
            config(),
            Arc::new(RecordingRunner::default()),
            events.clone(),
            Arc::new(TestClock::default()),
            FakeSource::new(vec![completed.clone()]),
        )
    };
    let service = build().await.expect("first restore");
    let ticket = service.ticket("tkt_2").await.expect("tkt_2");
    assert_eq!(ticket.state, TicketState::Succeeded { exit: 0 });
    assert_eq!(
        ticket.finished_ms,
        Some(400),
        "a terminal row keeps its finish time"
    );
    assert_eq!(
        ticket.output_path,
        Some(PathBuf::from("/tmp/lane-history.log")),
        "a terminal row keeps its captured output path"
    );
    assert!(
        events.ids().is_empty(),
        "history is read, never re-published: {:?}",
        events.ids()
    );

    // A second boot over the same source is idempotent and still silent.
    let second = build().await.expect("second restore");
    assert_eq!(
        second.ticket("tkt_2").await.expect("tkt_2").finished_ms,
        Some(400)
    );
    assert!(events.ids().is_empty(), "{:?}", events.ids());
    service.shutdown().await;
    second.shutdown().await;
}

/// A coalesced run survives one subscriber leaving, and is only cancelled once
/// nobody is left waiting for it.
#[tokio::test]
async fn unsubscribe_keeps_the_run_while_someone_still_wants_it() {
    let runner = BlockingRunner::new();
    let service = Arc::new(LaneService::new(
        config(),
        runner.clone(),
        Arc::new(NoEvents),
        Arc::new(TestClock::default()),
    ));

    let first = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo test -p oc-x", Some("rev1"), "ses_owner"),
        )
        .await
        .expect("owner");
    let joined = service
        .enqueue_detailed(
            LANE.to_owned(),
            ticket("cargo test -p oc-x", Some("rev1"), "ses_guest"),
        )
        .await
        .expect("subscriber");
    assert_eq!(joined.id, first.id);
    wait_started(&runner, 1).await;

    // The guest leaves: the owner still wants the run, so it stays alive.
    assert!(
        service
            .unsubscribe(&first.id, "ses_guest")
            .await
            .expect("unsubscribe guest")
    );
    assert_eq!(
        service.ticket(&first.id).await.expect("ticket").state,
        TicketState::Running
    );

    // The owner leaving with nobody else left cancels the run.
    assert!(
        !service
            .unsubscribe(&first.id, "ses_owner")
            .await
            .expect("unsubscribe owner")
    );
    assert_eq!(
        service.ticket(&first.id).await.expect("ticket").state,
        TicketState::Cancelled
    );
    runner.release(1);
    assert!(matches!(
        service.unsubscribe("missing", "ses").await,
        Err(oc_lanes::LaneError::UnknownTicket(_))
    ));
    service.shutdown().await;
}
