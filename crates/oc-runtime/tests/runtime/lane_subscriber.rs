//! Shared lane tickets must outlive the caller that joins them: a `shell` call
//! that times out on a shared ticket leaves the run alive for its owner, and
//! every subscriber that joins an identical submission receives the same
//! result while the runner executes the command once.
//!
//! The lane service already proves `unsubscribe` keeps a run while someone
//! still wants it (`oc-lanes/tests/restore.rs`). The tool test below goes
//! through the tool surface: two `shell` calls coalesce onto one ticket, the
//! subscriber times out, and the owner still collects the result. The service
//! test enqueues directly from three owners and checks the shared terminal
//! result.
//!
//! The runner is gated by a semaphore so the ticket stays `Running` until the
//! test releases it: no sleeps are used to decide outcomes, only to poll for a
//! state the test then asserts.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use oc_lanes::{
    BoxFuture, LaneConfig, LaneEvents, LaneRunner, LaneService, NewTicket, RunnerResult,
    SystemClock, Ticket, TicketState, classify,
};
use oc_runtime::LaneTools;
use serde_json::{Value, json};

use crate::support::*;

/// Builds a git repository in the harness work dir so the `cargo:` lane gets a
/// `tree_rev` and two identical submissions coalesce onto one ticket.
fn init_repository(work: &Path) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(work)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&["add", "hello.txt"]);
    git(&["commit", "--quiet", "-m", "initial"]);
}

/// Published tickets are irrelevant to this test: the assertions read the
/// service directly.
#[derive(Debug)]
struct IgnoreEvents;

impl LaneEvents for IgnoreEvents {
    fn changed(&self, _ticket: &Ticket) {}
}

/// Blocks every run until the test releases it, so a queued ticket becomes
/// `Running` and stays there while the subscriber's wait expires.
#[derive(Debug)]
struct GatedRunner {
    started: AtomicUsize,
    release: tokio::sync::Semaphore,
}

impl GatedRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: AtomicUsize::new(0),
            release: tokio::sync::Semaphore::new(0),
        })
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

impl LaneRunner for GatedRunner {
    fn run<'a>(&'a self, _ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        self.started.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let _permit = self.release.acquire().await.expect("the test releases it");
            RunnerResult::success()
        })
    }
}

/// Polls until the runner has begun; a missing start is a test failure, not a
/// hang.
async fn wait_started(runner: &GatedRunner, wanted: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while runner.started() < wanted {
        assert!(Instant::now() < deadline, "the runner never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The first `shell` call owns the ticket and keeps waiting with a generous
/// timeout; the second call joins it as a subscriber and times out after
/// 50 ms. Its `unsubscribe` must leave the owner's run running, and the owner
/// must still observe the successful result.
#[tokio::test]
async fn a_timed_out_subscriber_keeps_the_owner_run_alive() {
    let harness =
        Harness::new_with_setup(vec![], Arc::new(AllowAll), json!({}), None, init_repository).await;
    let runner = GatedRunner::new();
    let service = Arc::new(LaneService::new(
        LaneConfig::default(),
        runner.clone(),
        Arc::new(IgnoreEvents),
        Arc::new(SystemClock),
    ));
    let runtime = harness.runtime.clone().with_lanes(Arc::clone(&service));
    let shell = LaneTools::new(runtime)
        .registry()
        .get("shell")
        .cloned()
        .expect("the lane shell tool is registered");

    let command = "cargo test -p oc-runtime --lib";
    let request = classify(command, &harness.work).expect("cargo test is lane-routed");
    let owner_ctx = tool_context(&harness, &harness.session);
    let guest_ctx = tool_context(&harness, "ses_lane_guest");

    let owner_task = {
        let shell = Arc::clone(&shell);
        tokio::spawn(async move {
            shell
                .execute(&owner_ctx, json!({"command": command, "timeout": 60_000}))
                .await
        })
    };
    wait_started(&runner, 1).await;
    let lane_status = service.status(Some(&request.lane)).await;
    assert_eq!(
        lane_status[0].running, 1,
        "the owner's ticket must be running before the subscriber joins"
    );
    assert_eq!(
        lane_status[0].queued, 0,
        "two shell calls for the same work must leave exactly one ticket in the lane"
    );

    let error = shell
        .execute(&guest_ctx, json!({"command": command, "timeout": 50}))
        .await
        .expect_err("the subscriber's wait must time out");
    let metadata = error.metadata.expect("the timeout carries metadata");
    let ticket_id = metadata["ticket"]
        .as_str()
        .expect("the timeout names its ticket")
        .to_owned();
    assert_eq!(
        metadata["subscribed"],
        json!(true),
        "the second call must join the owner's ticket instead of enqueuing"
    );
    assert_eq!(metadata["status"], json!("timeout"));
    assert!(
        error.message.contains("the run continues"),
        "a leaving subscriber must hear that the run continues: {}",
        error.message
    );
    assert!(
        !error.message.contains("no longer running"),
        "the shared run must not be reported as stopped: {}",
        error.message
    );
    assert_eq!(
        service.ticket(&ticket_id).await.expect("ticket").state,
        TicketState::Running,
        "the subscriber's timeout must not cancel the owner's run"
    );

    runner.release();
    let finished = service
        .wait(&ticket_id, Duration::from_secs(10))
        .await
        .expect("the released run finishes");
    assert_eq!(finished.state, TicketState::Succeeded { exit: 0 });

    let owner_result = owner_task
        .await
        .expect("the owner task joins")
        .expect("the owner still gets its result");
    let owner_metadata: Value = owner_result
        .metadata
        .expect("the done result carries metadata");
    assert_eq!(owner_metadata["ticket"], json!(ticket_id));
    assert_eq!(owner_metadata["status"], json!("succeeded"));
    assert_eq!(
        service.ticket(&ticket_id).await.expect("ticket").state,
        TicketState::Succeeded { exit: 0 }
    );
    service.shutdown().await;
}

/// Three sessions submit the same `(lane, command, cwd, tree_rev)`; the second
/// and third join the active ticket instead of enqueuing, the subscriber list
/// is deduplicated, and every session collects the one shared terminal result
/// after the runner executes exactly once.
#[tokio::test]
async fn two_subscribers_receive_the_same_result() {
    let harness =
        Harness::new_with_setup(vec![], Arc::new(AllowAll), json!({}), None, init_repository).await;
    let runner = GatedRunner::new();
    let service = Arc::new(LaneService::new(
        LaneConfig::default(),
        runner.clone(),
        Arc::new(IgnoreEvents),
        Arc::new(SystemClock),
    ));

    let command = "cargo test -p oc-runtime --lib";
    let request = classify(command, &harness.work).expect("cargo test is lane-routed");
    let new_ticket = |owner: &str| NewTicket {
        command: command.to_owned(),
        cwd: harness.work.clone(),
        priority: 0,
        owner_session: Some(owner.to_owned()),
        label: None,
        tree_rev: Some("git:shared-result".to_owned()),
    };

    let owner = service
        .enqueue_detailed(request.lane.clone(), new_ticket("ses_lane_owner"))
        .await
        .expect("the owner enqueues");
    assert!(
        !owner.subscribed,
        "the first submission must create the ticket, not join one"
    );
    wait_started(&runner, 1).await;

    let mut subscribers = Vec::new();
    // The same session twice proves the subscriber list is deduplicated.
    for session in [
        "ses_lane_alpha",
        "ses_lane_alpha",
        "ses_lane_beta",
        harness.session.as_str(),
    ] {
        let joined = service
            .enqueue_detailed(request.lane.clone(), new_ticket(session))
            .await
            .expect("a subscriber enqueues");
        assert!(
            joined.subscribed,
            "{session} must subscribe to the active ticket"
        );
        assert_eq!(
            joined.id, owner.id,
            "{session} must share the owner's ticket id"
        );
        if !subscribers.contains(&session) {
            subscribers.push(session);
        }
    }

    let lane_status = service.status(Some(&request.lane)).await;
    assert_eq!(
        lane_status[0].running, 1,
        "three submissions must leave exactly one running ticket"
    );
    assert_eq!(
        lane_status[0].queued, 0,
        "coalescing must not enqueue copies"
    );
    let shared = service.ticket(&owner.id).await.expect("the shared ticket");
    let expected: Vec<String> = subscribers.iter().map(|id| (*id).to_owned()).collect();
    assert_eq!(
        shared.subscribers, expected,
        "every distinct subscriber must be recorded once, in subscription order"
    );

    runner.release();
    let mut finished = Vec::new();
    for session in std::iter::once("ses_lane_owner").chain(subscribers.iter().copied()) {
        let ticket = service
            .wait(&owner.id, Duration::from_secs(1))
            .await
            .unwrap_or_else(|error| panic!("{session} must collect the shared result: {error}"));
        finished.push((session, ticket));
    }
    for (session, ticket) in &finished {
        assert_eq!(
            ticket.state,
            TicketState::Succeeded { exit: 0 },
            "{session} must see the shared success"
        );
        assert_eq!(
            ticket.id, owner.id,
            "{session} must see the one coalesced ticket"
        );
        assert_eq!(
            ticket.output_path, finished[0].1.output_path,
            "{session} must see the same captured output"
        );
    }
    assert_eq!(
        runner.started(),
        1,
        "coalescing must not silently split the run"
    );
    service.shutdown().await;
}
