//! Output reader, exit watcher and process-tree teardown for shell commands.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

use super::super::util::{lock, now_ms};
use super::super::{DRAIN_WAIT, EXITED_LIMIT};
use super::{OUTCOME_EXITED, OUTCOME_KILLED, OUTCOME_TIMEOUT, Registry, Shell, Status};

pub(super) async fn read_output(shell: Arc<Shell>, mut reader: tokio::fs::File) {
    let mut buffer = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                if shell.process_done.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(count) => lock(&shell.output).append(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    shell.output_done.store(true, Ordering::Release);
    shell.output_notify.notify_one();
}

pub(super) async fn wait_for_exit(
    shell: Arc<Shell>,
    mut child: Child,
    registry: Weak<Mutex<Registry>>,
    timeout_after: Option<Duration>,
) {
    let status = if let Some(limit) = timeout_after {
        tokio::select! {
            result = child.wait() => result,
            () = tokio::time::sleep(limit) => {
                if shell.claim(OUTCOME_TIMEOUT) {
                    kill_tree(shell.pid).await;
                }
                child.wait().await
            }
        }
    } else {
        child.wait().await
    };

    let (exit, signal) = match status {
        Ok(status) => exit_status(status),
        Err(_) => (None, None),
    };
    let outcome = match shell.outcome.load(Ordering::SeqCst) {
        OUTCOME_TIMEOUT => Status::Timeout,
        OUTCOME_KILLED => Status::Killed,
        _ => {
            shell.outcome.store(OUTCOME_EXITED, Ordering::SeqCst);
            Status::Exited
        }
    };
    shell.process_done.store(true, Ordering::Release);
    wait_output(&shell, DRAIN_WAIT).await;

    let completed = now_ms();
    let exit = if outcome == Status::Exited {
        exit
    } else {
        None
    };
    {
        let mut info = lock(&shell.info);
        info.status = outcome;
        info.completed = Some(completed);
        info.exit = exit;
        info.signal = signal;
    }
    let mut event = Map::new();
    event.insert("id".to_owned(), json!(shell.id));
    if let Some(exit) = exit {
        event.insert("exit".to_owned(), json!(exit));
    }
    event.insert("status".to_owned(), json!(outcome.as_str()));
    shell.publish("shell.exited", Value::Object(event));

    if let Some(registry) = registry.upgrade() {
        retire(&registry, &shell.id).await;
    }
    shell.finished.store(true, Ordering::Release);
    shell.notify.notify_one();
}

async fn wait_output(shell: &Shell, limit: Duration) {
    if shell.output_done.load(Ordering::Acquire) {
        return;
    }
    let result = timeout(limit, async {
        while !shell.output_done.load(Ordering::Acquire) {
            shell.output_notify.notified().await;
        }
    })
    .await;
    let _ = result;
}

pub(super) async fn wait_finished(shell: &Shell, limit: Duration) {
    if shell.finished.load(Ordering::Acquire) {
        return;
    }
    let result = timeout(limit, async {
        while !shell.finished.load(Ordering::Acquire) {
            shell.notify.notified().await;
        }
    })
    .await;
    let _ = result;
}

/// Marks `id` as the newest exited shell and evicts the oldest beyond
/// [`EXITED_LIMIT`], deleting evicted capture files.
async fn retire(registry: &Arc<Mutex<Registry>>, id: &str) {
    let removed: Vec<Arc<Shell>> = {
        let mut state = lock(registry);
        if !state.shells.contains_key(id) {
            return;
        }
        state.exited.retain(|existing| existing != id);
        state.exited.push_back(id.to_owned());
        let mut removed = Vec::new();
        while state.exited.len() > EXITED_LIMIT {
            let Some(oldest) = state.exited.pop_front() else {
                break;
            };
            if let Some(shell) = state.shells.remove(&oldest) {
                removed.push(shell);
            }
        }
        removed
    };
    for shell in removed {
        let _ = tokio::fs::remove_file(&shell.file).await;
    }
}

/// Kills a process tree by pid. Windows uses `taskkill /T /F`; Unix signals the
/// process group created by the spawn.
pub(super) async fn kill_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        for signal in ["-TERM", "-KILL"] {
            let _ = Command::new("kill")
                .arg(signal)
                .arg(format!("-{pid}"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            if signal == "-TERM" {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

pub(super) fn kill_tree_blocking(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

fn exit_status(status: std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    if let Some(code) = status.code() {
        return (Some(code), None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (Some(128 + signal), Some(signal_name(signal).to_owned()));
        }
    }
    (None, None)
}

#[cfg(unix)]
fn signal_name(signal: i32) -> &'static str {
    match signal {
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => "SIGUNKNOWN",
    }
}
