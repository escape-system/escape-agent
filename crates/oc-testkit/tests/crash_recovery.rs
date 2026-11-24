//! Real process-kill crash recovery: a child process commits durable rows,
//! then the parent sends `SIGKILL` and reopens the same SQLite file.
//!
//! # What is actually proved here (and what is not)
//!
//! These are **store-level** assertions. `oc-testkit` depends on `oc-store`
//! only, so the test cannot run `oc-runtime`'s boot recovery
//! (`crates/oc-runtime/src/recover.rs`) in-process. What it does prove, with a
//! process that really died from signal 9:
//!
//! 1. committed rows survive the kill and are readable from a *new* process
//!    (the reopen only succeeds because the store's sidecar lock is taken over
//!    from a dead pid — `crates/oc-store/src/db.rs:264-277`);
//! 2. the interrupted turn is durably left with no terminal event and an
//!    incomplete assistant row, which is exactly the precondition recovery
//!    repairs (`recover.rs:281-360`) and the state that makes
//!    `has_terminal_execution` false (`recover.rs:257-277`), so recovery has
//!    exactly one terminal outcome (`interrupted`) to publish — never two
//!    (`recover.rs:59-69`);
//! 3. the control case — a turn whose terminal event *did* commit before the
//!    kill — keeps exactly one terminal event in the log, so the double-terminal
//!    guard has nothing to add;
//! 4. `PRAGMA quick_check` reports `ok`, i.e. the kill did not corrupt the file.
//!
//! Not proved here: that `Runtime::recover()` itself runs and repairs. That is
//! covered by `oc-runtime/tests/runtime/turn_core.rs:1342` (stale rows, no real
//! kill). See the handoff for this split.
//!
//! # How the child is produced
//!
//! The test binary re-executes itself (`std::env::current_exe()`) with
//! `OC_TESTKIT_KILL_CHILD=1` and an `--exact kill_child_entry` filter, so only
//! the entry test runs in the child. That entry opens a real `Store`, commits a
//! real open (or settled) turn, writes a ready marker and then parks on
//! `std::future::pending()`, keeping the store — and its WAL — open. The
//! parent polls the marker (bounded), sends `SIGKILL` through `/bin/kill`, waits
//! for the child, and reopens the file. No `unsafe`, no `libc`, no new binary.
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use oc_store::{MessageQuery, NewEvent, NewMessage, NewSession, Order, Store, StoredEvent};
use serde_json::{Value, json};

/// Set for the re-executed child only.
const CHILD_ENV: &str = "OC_TESTKIT_KILL_CHILD";
/// Database file the child writes and the parent reopens.
const DB_ENV: &str = "OC_TESTKIT_KILL_DB";
/// Marker the child writes only after its commits returned.
const MARKER_ENV: &str = "OC_TESTKIT_KILL_MARKER";
/// Which turn shape the child commits: `open` or `settled`.
const MODE_ENV: &str = "OC_TESTKIT_KILL_MODE";

const MODE_OPEN: &str = "open";
const MODE_SETTLED: &str = "settled";

const SESSION: &str = "ses_sigkill";
const ASSISTANT: &str = "msg_sigkill_assistant";
const EVT_STARTED: &str = "evt_sigkill_started";
const EVT_SUCCEEDED: &str = "evt_sigkill_succeeded";
const CREATED: i64 = 1_700_000_000_000;

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const READY_POLL: Duration = Duration::from_millis(20);

/// Event kinds that close a session's execution. A second one for the same
/// session contradicts the first (`crates/oc-runtime/src/recover.rs:18-22`).
const TERMINAL_EXECUTIONS: [&str; 3] = [
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
];

/// Child entry point. A no-op in the parent test process; in the re-executed
/// child it commits the turn and then blocks forever without releasing the
/// store, so the parent's `SIGKILL` interrupts a live writer.
#[test]
fn kill_child_entry() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let mode = std::env::var(MODE_ENV).expect("child mode");
    let db = PathBuf::from(std::env::var(DB_ENV).expect("child db path"));
    let marker = PathBuf::from(std::env::var(MARKER_ENV).expect("child marker path"));
    let directory = db
        .parent()
        .expect("db has a parent directory")
        .display()
        .to_string();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("child runtime");

    runtime.block_on(async move {
        let store = Store::open(&db).await.expect("child opens store");
        store
            .create_session(NewSession {
                id: SESSION.to_owned(),
                parent_id: None,
                project_id: "prj_sigkill".to_owned(),
                directory,
                title: Some("sigkill".to_owned()),
                agent: Some("engineer".to_owned()),
                model: Some(json!({"providerID": "test", "id": "alpha"})),
                metadata: None,
                permissions: None,
                time_created: CREATED,
            })
            .await
            .expect("child creates session");
        store
            .append_event(NewEvent {
                id: EVT_STARTED.to_owned(),
                aggregate_id: SESSION.to_owned(),
                kind: "session.execution.started".to_owned(),
                data: json!({"sessionID": SESSION}),
                durable: true,
                version: 1,
                created: CREATED,
            })
            .await
            .expect("child appends started");
        store
            .append_message(NewMessage {
                id: ASSISTANT.to_owned(),
                session_id: SESSION.to_owned(),
                kind: "assistant".to_owned(),
                data: assistant(false),
                time_created: CREATED,
            })
            .await
            .expect("child appends assistant");

        if mode == MODE_SETTLED {
            // The product order: the message row commits first, the terminal
            // event commits after it (`persist_assistant`, `turn/stream.rs:135`).
            store
                .update_message(SESSION, ASSISTANT, assistant(true))
                .await
                .expect("child settles assistant");
            store
                .append_event(NewEvent {
                    id: EVT_SUCCEEDED.to_owned(),
                    aggregate_id: SESSION.to_owned(),
                    kind: "session.execution.succeeded".to_owned(),
                    data: json!({"sessionID": SESSION}),
                    durable: true,
                    version: 1,
                    created: CREATED,
                })
                .await
                .expect("child appends succeeded");
        }

        // Both writes above returned, i.e. their transactions committed. Only
        // now is the marker visible.
        std::fs::write(&marker, b"ready").expect("child writes marker");

        // Hold the store (and its WAL) open until the parent kills us.
        std::future::pending::<()>().await;
        panic!("the child process was not killed");
    });
}

/// A turn body in the wire shape the store and recovery use.
///
/// `settled == false` is the interrupted shape: no `time.completed` and a tool
/// part still `running`, which `repair_assistant` (`recover.rs:281-360`)
/// repairs and `nothing else does`.
fn assistant(settled: bool) -> Value {
    let mut body = json!({
        "time": {"created": CREATED},
        "agent": "engineer",
        "model": {"providerID": "test", "id": "alpha"},
        "content": [{
            "type": "tool",
            "id": "call_sigkill",
            "name": "read",
            "state": {"status": "running", "input": {"path": "hello.txt"}},
            "time": {"created": CREATED}
        }]
    });
    if settled {
        body["time"]["completed"] = json!(CREATED + 5);
        body["finish"] = json!("stop");
        body["content"][0]["state"] = json!({
            "status": "completed",
            "input": {"path": "hello.txt"},
            "content": [{"type": "text", "text": "hello world"}]
        });
        body["content"][0]["time"]["completed"] = json!(CREATED + 5);
    }
    body
}

/// A spawned child that is killed and reaped on drop, so a failing assertion
/// cannot leak a process.
#[derive(Debug)]
struct KillChild {
    child: Child,
    reaped: bool,
}

impl KillChild {
    fn spawn(mode: &str, db: &Path, marker: &Path) -> Self {
        let exe = std::env::current_exe().expect("current test executable");
        let child = Command::new(exe)
            .args(["--exact", "--test-threads", "1", "kill_child_entry"])
            .env(CHILD_ENV, "1")
            .env(MODE_ENV, mode)
            .env(DB_ENV, db)
            .env(MARKER_ENV, marker)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the kill child");
        Self {
            child,
            reaped: false,
        }
    }

    /// `SIGKILL` the child through `/bin/kill` and reap it.
    ///
    /// Asserts the target is still alive first: `kill -0` succeeding is what
    /// makes the kill a kill of a *live* process holding the store open, not
    /// of an already-exited one. (`kill -0` is the same liveness probe
    /// `oc-store` uses to take over a stale lock, `db.rs:288-295`.)
    fn sigkill(&mut self) -> ExitStatus {
        let pid = self.child.id();
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .expect("run kill -0");
        assert!(
            alive.success(),
            "child {pid} was already gone before SIGKILL; \
             the kill would prove nothing"
        );
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("run kill -9");
        assert!(status.success(), "kill -9 {pid} failed");
        let status = self.child.wait().expect("reap the killed child");
        self.reaped = true;
        status
    }
}

impl Drop for KillChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Waits for the child's ready marker, bounded by [`READY_TIMEOUT`].
async fn wait_ready(marker: &Path) {
    let started = Instant::now();
    while !marker.exists() {
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "child never wrote {} within {READY_TIMEOUT:?}",
            marker.display()
        );
        tokio::time::sleep(READY_POLL).await;
    }
}

/// `PRAGMA quick_check` on the reopened file.
fn quick_check(path: &Path) -> String {
    let connection = rusqlite::Connection::open(path).expect("open sqlite for quick_check");
    connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .expect("PRAGMA quick_check")
}

fn terminal_count(events: &[StoredEvent]) -> usize {
    events
        .iter()
        .filter(|event| TERMINAL_EXECUTIONS.contains(&event.kind.as_str()))
        .count()
}

async fn events(store: &Store) -> Vec<StoredEvent> {
    store
        .events_after(SESSION, 0, 100)
        .await
        .expect("read the durable event log")
}

/// K1 + K2: a turn that was still open when the process died is durable,
/// readable, has no terminal event, and the file is uncorrupted.
#[tokio::test]
async fn sigkill_leaves_an_open_turn_for_boot_recovery() {
    let root = tempfile::tempdir().expect("temp dir");
    let db = root.path().join("sigkill-open.db");
    let marker = root.path().join("ready-open");
    let mut child = KillChild::spawn(MODE_OPEN, &db, &marker);
    wait_ready(&marker).await;

    let status = child.sigkill();
    assert_eq!(
        status.signal(),
        Some(9),
        "the child must die from SIGKILL, not exit on its own: {status:?}"
    );

    // Reopening only succeeds because the sidecar lock still names a dead pid
    // (`db.rs:264-277`); a live owner would return a Conflict.
    let store = Store::open(&db).await.expect("reopen after SIGKILL");
    assert_eq!(
        quick_check(&db),
        "ok",
        "SIGKILL must not corrupt the database"
    );

    // K1a: the interrupted turn is still there, still non-terminal.
    let message = store
        .get_message(SESSION, ASSISTANT)
        .await
        .expect("read the assistant row")
        .expect("the committed assistant row survived SIGKILL");
    assert_eq!(message["type"], "assistant");
    assert_eq!(message["sessionID"], SESSION);
    assert!(
        message["time"]["completed"].is_null(),
        "an open turn must have no completion stamp: {message}"
    );
    assert_eq!(message["content"][0]["state"]["status"], "running");

    let assistant_rows = store
        .list_messages(MessageQuery {
            session_id: SESSION.to_owned(),
            limit: 50,
            order: Order::Asc,
            cursor: None,
            kind: Some("assistant".to_owned()),
        })
        .await
        .expect("list assistant rows");
    assert_eq!(
        assistant_rows.items.len(),
        1,
        "exactly one turn row was committed"
    );

    // K1b: no terminal event committed before the kill, so boot recovery has
    // exactly one outcome to publish (interrupted) and cannot publish a second.
    let log = events(&store).await;
    assert_eq!(
        log.iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["session.execution.started"],
        "the log holds the started event and nothing terminal"
    );
    assert_eq!(terminal_count(&log), 0, "no terminal event was committed");
    assert_eq!(log[0].version, 1);

    // Reopening a store publishes nothing: recovery is a runtime step, so the
    // facts above are still the ones boot recovery will read.
    let after_reopen = events(&store).await;
    assert_eq!(after_reopen.len(), log.len());
}

/// K3: a turn whose terminal event *did* commit before the kill keeps exactly
/// one terminal outcome, so recovery's double-terminal guard has nothing to add.
#[tokio::test]
async fn sigkill_preserves_a_single_terminal_outcome() {
    let root = tempfile::tempdir().expect("temp dir");
    let db = root.path().join("sigkill-settled.db");
    let marker = root.path().join("ready-settled");
    let mut child = KillChild::spawn(MODE_SETTLED, &db, &marker);
    wait_ready(&marker).await;

    let status = child.sigkill();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9),
        "the child must die from SIGKILL, not exit on its own: {status:?}"
    );

    let store = Store::open(&db).await.expect("reopen after SIGKILL");
    assert_eq!(
        quick_check(&db),
        "ok",
        "SIGKILL must not corrupt the database"
    );

    let message = store
        .get_message(SESSION, ASSISTANT)
        .await
        .expect("read the assistant row")
        .expect("the committed assistant row survived SIGKILL");
    assert_eq!(
        message["time"]["completed"],
        CREATED + 5,
        "a settled turn keeps its completion stamp"
    );
    assert_eq!(message["finish"], "stop");

    let events = events(&store).await;
    let kinds = events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["session.execution.started", "session.execution.succeeded"],
        "the committed terminal event survived the kill"
    );
    assert_eq!(
        terminal_count(&events),
        1,
        "exactly one terminal outcome, never two"
    );
}
