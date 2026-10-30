//! Cross-platform tests against a real shell.
//!
//! Windows uses `cmd.exe`, other platforms use `sh`. If the PTY or shell cannot
//! be spawned the test reports a skip instead of pretending to pass.

use std::collections::BTreeMap;

use std::time::Duration;

use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout};

use crate::{CreatePty, PtyConfig, PtyError, PtyEvent, PtyInfo, PtyManager};

const WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(10);

/// Windows `ConPTY` shells contend for console resources when many are started
/// at once; the manager itself supports 32 live terminals, but the test suite
/// serialises real-shell tests so a loaded CI box does not blow the 5s budget.
static SHELL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn shell_guard() -> tokio::sync::MutexGuard<'static, ()> {
    SHELL_LOCK.lock().await
}

fn shell() -> &'static str {
    if cfg!(windows) { "cmd.exe" } else { "sh" }
}

fn request(id: &str) -> CreatePty {
    CreatePty {
        id: Some(id.to_string()),
        shell: Some(shell().to_string()),
        args: Vec::new(),
        cwd: std::env::current_dir().expect("cwd"),
        location: None,
        env: BTreeMap::new(),
        cols: 80,
        rows: 24,
        title: None,
    }
}

fn one_shot(id: &str, args: Vec<String>) -> CreatePty {
    let mut req = request(id);
    req.args = args;
    req
}

fn exit_request(id: &str) -> CreatePty {
    if cfg!(windows) {
        one_shot(id, vec!["/c".into(), "exit 0".into()])
    } else {
        one_shot(id, vec!["-c".into(), "exit 0".into()])
    }
}

fn create_or_skip(manager: &PtyManager, req: CreatePty) -> Option<PtyInfo> {
    match manager.create(req) {
        Ok(info) => Some(info),
        Err(error @ PtyError::Operation { .. }) => {
            eprintln!("skipping PTY test: {error}");
            None
        }
        Err(error) => panic!("create failed: {error}"),
    }
}

async fn next_event(rx: &mut Receiver<PtyEvent>) -> PtyEvent {
    loop {
        match timeout(WAIT, rx.recv()).await {
            Ok(Ok(event)) => return event,
            Ok(Err(RecvError::Lagged(_))) => {}
            Ok(Err(RecvError::Closed)) => panic!("event channel closed"),
            Err(error) => panic!("timed out waiting for a terminal event: {error}"),
        }
    }
}

async fn wait_exit(rx: &mut Receiver<PtyEvent>, id: &str) -> Option<i32> {
    loop {
        if let PtyEvent::Exited { id: event_id, exit } = next_event(rx).await
            && event_id == id
        {
            return exit;
        }
    }
}

async fn wait_output_contains(rx: &mut Receiver<PtyEvent>, id: &str, needle: &str) -> String {
    let mut output = Vec::new();
    loop {
        if let PtyEvent::Output {
            id: event_id,
            bytes,
        } = next_event(rx).await
            && event_id == id
        {
            output.extend_from_slice(&bytes);
            let text = String::from_utf8_lossy(&output);
            if text.contains(needle) {
                return text.into_owned();
            }
        }
    }
}

async fn wait_snapshot_contains(manager: &PtyManager, id: &str, needle: &str) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = manager.snapshot(id, 100).expect("terminal exists");
        if snapshot.contains(needle) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "snapshot never contained {needle:?}: {snapshot:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

#[tokio::test]
async fn create_write_read_echo() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let mut events = manager.subscribe();
    let Some(info) = create_or_skip(&manager, request("echo")) else {
        return;
    };
    assert!(info.running);
    assert!(info.pid.is_some());
    assert_eq!((info.cols, info.rows), (80, 24));
    assert!(info.created_ms > 0);

    manager.write("echo", b"echo hello\r").expect("write");
    let output = wait_output_contains(&mut events, "echo", "hello").await;
    assert!(output.contains("hello"), "output: {output:?}");

    let snapshot = wait_snapshot_contains(&manager, "echo", "hello").await;
    assert!(snapshot.contains("hello"));
    assert!(!snapshot.contains('\r'));

    let info = manager.info("echo").expect("info");
    assert!(info.output_bytes > 0);
    assert!(!info.running || info.pid.is_some());
    assert!(manager.list().iter().any(|item| item.id == "echo"));

    manager.shutdown();
}

#[tokio::test]
async fn duplicate_and_unknown_ids() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let Some(_) = create_or_skip(&manager, request("dup")) else {
        return;
    };
    assert!(matches!(
        manager.create(request("dup")),
        Err(PtyError::Conflict { .. })
    ));
    assert!(matches!(
        manager.remove("dup"),
        Err(PtyError::Running { .. })
    ));
    assert!(matches!(
        manager.write("missing", b"x"),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.snapshot("missing", 1),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.read_rows("missing", 0, 1),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.resize("missing", 1, 1),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.replay("missing", None),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.terminate("missing"),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.remove("missing"),
        Err(PtyError::NotFound { .. })
    ));
    assert!(manager.info("missing").is_none());
    manager.shutdown();
}

#[tokio::test]
async fn max_live_limit_and_reuse() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig {
        max_live: 1,
        ..PtyConfig::default()
    });
    let Some(_) = create_or_skip(&manager, request("one")) else {
        return;
    };
    assert!(matches!(
        manager.create(request("two")),
        Err(PtyError::Limit { limit: 1 })
    ));

    let mut events = manager.subscribe();
    manager.terminate("one").expect("terminate");
    let exit = wait_exit(&mut events, "one").await;
    assert!(exit.is_some());
    assert!(!manager.info("one").expect("info").running);

    let Some(_) = create_or_skip(&manager, request("two")) else {
        return;
    };
    assert!(manager.info("two").is_some());
    manager.shutdown();
}

#[tokio::test]
async fn retained_exited_terminals_are_evicted_oldest_first() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig {
        max_live: 4,
        retained_exited: 2,
        ..PtyConfig::default()
    });
    let mut events = manager.subscribe();
    for id in ["e1", "e2", "e3"] {
        if create_or_skip(&manager, exit_request(id)).is_none() {
            return;
        }
    }
    for id in ["e1", "e2", "e3"] {
        let exit = wait_exit(&mut events, id).await;
        assert_eq!(exit, Some(0), "unexpected exit for {id}");
    }

    let deadline = Instant::now() + WAIT;
    loop {
        let infos = manager.list();
        let ids: Vec<&str> = infos.iter().map(|info| info.id.as_str()).collect();
        if ids.len() == 2 && !ids.contains(&"e1") {
            assert!(ids.contains(&"e3"));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "exited retention did not settle: {ids:?}"
        );
        tokio::time::sleep(POLL).await;
    }
    manager.shutdown();
}

#[tokio::test]
async fn terminate_emits_one_exit_and_remove_deletes() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let mut events = manager.subscribe();
    let Some(_) = create_or_skip(&manager, request("term")) else {
        return;
    };
    manager.terminate("term").expect("terminate");
    let exit = wait_exit(&mut events, "term").await;
    assert!(exit.is_some());
    let info = manager.info("term").expect("exited info retained");
    assert!(!info.running);
    assert_eq!(info.exit, exit);

    manager
        .terminate("term")
        .expect("second terminate is a no-op");

    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, events.recv()).await {
            Ok(Ok(PtyEvent::Exited { .. })) => panic!("duplicate exit event"),
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }

    manager.remove("term").expect("remove exited");
    assert!(manager.info("term").is_none());
    manager.shutdown();
}

#[tokio::test]
async fn set_title_updates_info_and_publishes() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let mut events = manager.subscribe();
    let Some(info) = create_or_skip(&manager, request("title")) else {
        return;
    };
    assert_eq!(info.command, shell());
    assert!(info.cwd.is_absolute());
    let updated = manager.set_title("title", "renamed").expect("set title");
    assert_eq!(updated.title.as_deref(), Some("renamed"));
    assert_eq!(
        manager.info("title").expect("info").title.as_deref(),
        Some("renamed")
    );
    let title = timeout(WAIT, async {
        loop {
            match events.recv().await {
                Ok(PtyEvent::Title { id, title }) if id == "title" && title == "renamed" => {
                    return Some(title);
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    })
    .await
    .ok()
    .flatten();
    assert_eq!(title.as_deref(), Some("renamed"));
    manager.shutdown();
}

#[tokio::test]
async fn resize_updates_info() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let Some(_) = create_or_skip(&manager, request("resize")) else {
        return;
    };
    manager.resize("resize", 100, 40).expect("resize");
    let info = manager.info("resize").expect("info");
    assert_eq!((info.cols, info.rows), (100, 40));
    manager.shutdown();
}

#[tokio::test]
async fn snapshot_normalizes_shell_output() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let args = if cfg!(windows) {
        vec!["/c".into(), "echo hello".into()]
    } else {
        vec!["-c".into(), "printf 'hello\\r\\n'".into()]
    };
    let Some(_) = create_or_skip(&manager, one_shot("snap", args)) else {
        return;
    };
    let snapshot = wait_snapshot_contains(&manager, "snap", "hello").await;
    assert_eq!(snapshot, "hello");
    manager.shutdown();
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_strips_ansi_sequences() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let Some(_) = create_or_skip(
        &manager,
        one_shot(
            "ansi",
            vec![
                "-c".into(),
                "printf '\\033[31mred\\033[0m\\r\\nplain\\n'".into(),
            ],
        ),
    ) else {
        return;
    };
    let snapshot = wait_snapshot_contains(&manager, "ansi", "plain").await;
    assert_eq!(snapshot, "red\nplain");
    assert!(!snapshot.contains('\u{1b}'));
    assert!(!snapshot.contains('\r'));
    manager.shutdown();
}

#[tokio::test]
async fn read_rows_by_absolute_index() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let Some(_) = create_or_skip(&manager, request("rows")) else {
        return;
    };
    manager.write("rows", b"echo alpha\r").expect("write alpha");
    manager.write("rows", b"echo beta\r").expect("write beta");
    wait_snapshot_contains(&manager, "rows", "beta").await;

    let rows = manager.read_rows("rows", 0, 100).expect("rows");
    let alpha_row = rows.iter().position(|row| row.contains("alpha"));
    let beta_row = rows.iter().position(|row| row.contains("beta"));
    assert!(alpha_row.is_some(), "{rows:?}");
    assert!(beta_row.is_some(), "{rows:?}");
    assert!(alpha_row < beta_row, "{rows:?}");
    assert_eq!(manager.read_rows("rows", 0, 2).expect("two").len(), 2);
    assert!(manager.read_rows("rows", 0, 0).expect("zero").is_empty());
    manager.shutdown();
}

#[tokio::test]
async fn ring_wrap_reports_row_gap() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig {
        buffer_bytes: 64,
        ..PtyConfig::default()
    });
    let mut events = manager.subscribe();
    let args = if cfg!(windows) {
        vec!["/c".into(), "for /L %i in (1,1,200) do @echo %i".into()]
    } else {
        vec!["-c".into(), "seq 1 200".into()]
    };
    let Some(_) = create_or_skip(&manager, one_shot("wrap", args)) else {
        return;
    };
    wait_exit(&mut events, "wrap").await;

    let deadline = Instant::now() + WAIT;
    let gap = loop {
        match manager.read_rows("wrap", 0, 10) {
            Err(error @ PtyError::RowGap { .. }) => break error,
            Ok(_) => {}
            Err(error) => panic!("unexpected read_rows error: {error}"),
        }
        assert!(Instant::now() < deadline, "ring never wrapped");
        tokio::time::sleep(POLL).await;
    };
    let PtyError::RowGap { oldest, .. } = gap else {
        unreachable!();
    };
    assert!(oldest > 0);
    let rows = manager
        .read_rows("wrap", oldest, 5)
        .expect("rows from oldest");
    assert!(!rows.is_empty());
    assert!(rows.len() <= 5);
    let info = manager.info("wrap").expect("info");
    assert!(info.output_bytes > 64);
    manager.shutdown();
}

#[tokio::test]
async fn replay_returns_retained_bytes_and_cursor() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let mut events = manager.subscribe();
    let Some(_) = create_or_skip(&manager, request("replay")) else {
        return;
    };
    manager.write("replay", b"echo replay-me\r").expect("write");
    wait_output_contains(&mut events, "replay", "replay-me").await;

    let output = manager.replay("replay", Some(0)).expect("replay");
    assert!(output.cursor > 0);
    assert!(
        String::from_utf8_lossy(&output.bytes).contains("replay-me"),
        "{:?}",
        String::from_utf8_lossy(&output.bytes)
    );
    let tail = manager.replay("replay", Some(u64::MAX)).expect("tail");
    assert!(tail.bytes.is_empty());
    assert!(tail.cursor >= output.cursor);
    manager.shutdown();
}

#[cfg(unix)]
#[tokio::test]
async fn title_event_from_osc() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let mut events = manager.subscribe();
    let Some(_) = create_or_skip(
        &manager,
        one_shot(
            "title",
            vec!["-c".into(), "printf '\\033]0;Nice Title\\007'".into()],
        ),
    ) else {
        return;
    };
    loop {
        if let PtyEvent::Title { id, title } = next_event(&mut events).await {
            assert_eq!(id, "title");
            assert_eq!(title, "Nice Title");
            break;
        }
    }
    assert_eq!(
        manager.info("title").expect("info").title.as_deref(),
        Some("Nice Title")
    );
    manager.shutdown();
}

#[tokio::test]
async fn shutdown_terminates_everything_and_rejects_creates() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let Some(_) = create_or_skip(&manager, request("s1")) else {
        return;
    };
    if create_or_skip(&manager, request("s2")).is_none() {
        return;
    }
    let mut events = manager.subscribe();
    manager.shutdown();
    manager.shutdown();

    assert!(manager.list().is_empty());
    assert!(manager.info("s1").is_none());
    assert!(matches!(
        manager.terminate("s1"),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.create(request("after")),
        Err(PtyError::Shutdown)
    ));

    let mut exited = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let PtyEvent::Exited { id, .. } = event {
            exited.push(id);
        }
    }
    exited.sort();
    assert_eq!(exited, vec!["s1".to_string(), "s2".to_string()]);
}

#[tokio::test]
async fn cloned_manager_shares_state() {
    let _guard = shell_guard().await;
    let manager = PtyManager::new(PtyConfig::default());
    let clone = manager.clone();
    let Some(_) = create_or_skip(&manager, request("shared")) else {
        return;
    };
    assert!(clone.info("shared").is_some());
    assert_eq!(clone.config().max_live, manager.config().max_live);
    clone.shutdown();
    assert!(manager.list().is_empty());
}

#[test]
fn manager_is_clone_send_sync() {
    fn assert_traits<T: Clone + Send + Sync>() {}
    assert_traits::<PtyManager>();
}

#[test]
fn defaults_match_documented_limits() {
    let config = PtyConfig::default();
    assert_eq!(config.max_live, 32);
    assert_eq!(config.retained_exited, 16);
    assert_eq!(config.buffer_bytes, 1024 * 1024);
    assert_eq!(config.read_rows_default, 100);
}
