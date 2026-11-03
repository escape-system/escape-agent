#[cfg(windows)]
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;

use oc_lanes::{BoxFuture, CancellationTokens, LaneRunner, LaneService, RunnerResult, Ticket};
use oc_tools::util;
use tokio::process::Command;

use super::LANE_DIR;

// ---------------------------------------------------------------------------
// Process runner
// ---------------------------------------------------------------------------

/// [`LaneRunner`] that spawns the platform shell in `ticket.cwd`.
///
/// stdout and stderr share one temp file under `<temp>/opencode-lanes`. The
/// returned [`RunnerResult::output_path`] points at that file so tools can read
/// the tail without polling the process.
///
/// The runner needs the service's cancellation table. Because
/// [`LaneService::new`] owns its table and requires a runner at construction,
/// hosts that build the service themselves use [`Self::unbound`] and then
/// [`Self::bind`] the finished service. Hosts that already have a table (or a
/// service) use [`Self::new`] / [`Self::for_service`].
#[derive(Debug, Clone, Default)]
pub struct ProcessLaneRunner {
    tokens: TokenSlot,
}

type TokenSlot = std::sync::Arc<std::sync::Mutex<Option<CancellationTokens>>>;

impl ProcessLaneRunner {
    /// Builds a runner that observes the given cancellation-token table.
    #[must_use]
    pub fn new(tokens: CancellationTokens) -> Self {
        Self {
            tokens: std::sync::Arc::new(std::sync::Mutex::new(Some(tokens))),
        }
    }

    /// Builds a runner before its service exists; call [`Self::bind`] after
    /// constructing the service.
    #[must_use]
    pub fn unbound() -> Self {
        Self::default()
    }

    /// Builds a runner for an existing service, using its token table.
    #[must_use]
    pub fn for_service(service: &LaneService) -> Self {
        Self::new(service.tokens())
    }

    /// Attaches the cancellation table of a service constructed with this
    /// runner. Safe to call again to re-bind.
    pub fn bind(&self, service: &LaneService) {
        *self
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(service.tokens());
    }

    fn token_table(&self) -> CancellationTokens {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_default()
    }
}

impl LaneRunner for ProcessLaneRunner {
    #[allow(clippy::too_many_lines)] // Spawn, capture and cancellation are one flow.
    fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
        Box::pin(async move {
            let output_path = match output_path(&ticket.id) {
                Ok(path) => path,
                Err(error) => {
                    return RunnerResult {
                        exit: 127,
                        message: Some(format!("Unable to create lane output file: {error}")),
                        output_path: None,
                    };
                }
            };
            let file = match OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&output_path)
            {
                Ok(file) => file,
                Err(error) => {
                    return RunnerResult {
                        exit: 127,
                        message: Some(format!("Unable to create lane output file: {error}")),
                        output_path: Some(output_path),
                    };
                }
            };
            let stdout = match file.try_clone() {
                Ok(handle) => handle,
                Err(error) => {
                    return RunnerResult {
                        exit: 127,
                        message: Some(format!("Unable to capture lane output: {error}")),
                        output_path: Some(output_path),
                    };
                }
            };
            let stderr = match stdout.try_clone() {
                Ok(handle) => handle,
                Err(error) => {
                    return RunnerResult {
                        exit: 127,
                        message: Some(format!("Unable to capture lane output: {error}")),
                        output_path: Some(output_path),
                    };
                }
            };
            drop(file);

            let (shell, kind) = select_shell();
            let mut command = Command::new(&shell);
            kind.apply(&mut command, &shell, &ticket.command);
            command
                .current_dir(&ticket.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .env("TERM", "xterm-256color")
                .env("OPENCODE", "1")
                .env("AGENT", "1")
                .env(
                    "OPENCODE_SESSION_ID",
                    ticket.owner_session.clone().unwrap_or_default(),
                )
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    return RunnerResult {
                        exit: 127,
                        message: Some(format!(
                            "Unable to execute command `{}`: {error}",
                            ticket.command
                        )),
                        output_path: Some(output_path),
                    };
                }
            };
            let pid = child.id().unwrap_or(0);

            let mut waited = None;
            if let Some(token) = self.token_table().get(&ticket.id) {
                tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        kill_tree(pid).await;
                    }
                    result = child.wait() => waited = Some(result),
                }
            } else {
                waited = Some(child.wait().await);
            }

            let status = match waited {
                Some(Ok(status)) => status,
                Some(Err(error)) => {
                    return RunnerResult {
                        exit: 127,
                        message: Some(format!("Unable to wait for command: {error}")),
                        output_path: Some(output_path),
                    };
                }
                None => {
                    // Cancelled while running: make sure the process is gone
                    // before returning, otherwise the ticket could finish while
                    // its children still hold the output file.
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return RunnerResult {
                        exit: 130,
                        message: Some("lane ticket was cancelled".to_owned()),
                        output_path: Some(output_path),
                    };
                }
            };

            RunnerResult {
                exit: exit_code(status),
                message: None,
                output_path: Some(output_path),
            }
        })
    }
}

/// Combined temp file path for one ticket. The directory is created on demand.
fn output_path(ticket_id: &str) -> io::Result<PathBuf> {
    let directory = std::env::temp_dir().join(LANE_DIR);
    fs::create_dir_all(&directory)?;
    let safe: String = ticket_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    Ok(directory.join(format!("lane-{safe}-{:016x}.log", util::unique())))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

/// Kills a process tree by pid. Windows uses `taskkill /T /F`; Unix signals the
/// process group created by [`ProcessLaneRunner`].
async fn kill_tree(pid: u32) {
    if pid <= 1 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        for signal in ["-TERM", "-KILL"] {
            let _ = Command::new("kill")
                .arg(signal)
                .arg(format!("-{pid}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Posix,
    #[cfg(windows)]
    PowerShell,
    #[cfg(windows)]
    Cmd,
}

impl ShellKind {
    fn apply(self, command: &mut Command, shell: &Path, source: &str) {
        match self {
            #[cfg(windows)]
            Self::Cmd => {
                // cmd.exe has no pipefail: `exit` stays the last segment's
                // status, matching a normal cmd session.
                command.arg("/c").arg(source);
            }
            #[cfg(windows)]
            Self::PowerShell => {
                // PowerShell has no pipefail either; `$LASTEXITCODE` covers
                // only the last native command, so the reported exit is the
                // final pipeline stage's status.
                command
                    .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                    .arg(source);
            }
            Self::Posix => {
                // Without pipefail a pipeline reports the *last* segment's
                // status, so `cargo clippy … | tail` would finish as a success
                // even when clippy failed. The ticket exit code is what the
                // lane reports to callers, so it must reflect a failed segment.
                if util::shell_supports_pipefail(shell) {
                    command.args(["-o", "pipefail", "-c"]).arg(source);
                } else {
                    // A POSIX sh without pipefail support keeps the historical
                    // last-segment behaviour rather than refusing to run.
                    command.arg("-c").arg(source);
                }
            }
        }
    }
}

fn select_shell() -> (PathBuf, ShellKind) {
    #[cfg(windows)]
    {
        if let Some(path) = find_in_path("pwsh") {
            return (path, ShellKind::PowerShell);
        }
        if let Some(path) = find_in_path("powershell") {
            return (path, ShellKind::PowerShell);
        }
        if let Some(git) = find_in_path("git")
            && let Some(parent) = git.parent().and_then(Path::parent)
        {
            let bash = parent.join("bin").join("bash.exe");
            if bash.is_file() {
                return (bash, ShellKind::Posix);
            }
        }
        if let Some(comspec) = std::env::var_os("COMSPEC") {
            return (PathBuf::from(comspec), ShellKind::Cmd);
        }
        (PathBuf::from("cmd.exe"), ShellKind::Cmd)
    }
    #[cfg(unix)]
    {
        if let Some(shell) = std::env::var_os("SHELL") {
            let path = PathBuf::from(shell);
            if allowed_shell(&path) && path.is_file() {
                return (path, ShellKind::Posix);
            }
        }
        for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/zsh", "/bin/sh"] {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return (path, ShellKind::Posix);
            }
        }
        (PathBuf::from("/bin/sh"), ShellKind::Posix)
    }
}

#[cfg(unix)]
fn allowed_shell(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(name.as_str(), "bash" | "sh" | "zsh" | "ksh")
}

/// Locate an executable on `PATH`. Windows shell discovery is the only caller:
/// on unix the shell is chosen by absolute path from `$SHELL` or a fixed list.
#[cfg(windows)]
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path: OsString = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if Path::new(name).extension().is_none() {
                for extension in ["exe", "cmd", "bat", "com"] {
                    let candidate = directory.join(format!("{name}.{extension}"));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use oc_lanes::{LaneRunner, TicketState};

    fn ticket(command: &str) -> Ticket {
        let cwd = std::env::temp_dir();
        Ticket {
            id: format!("tkt_{:016x}", util::unique()),
            lane: "cargo:test".to_owned(),
            command: command.to_owned(),
            cwd,
            priority: 0,
            owner_session: None,
            state: TicketState::Queued,
            queued_ms: 0,
            started_ms: None,
            finished_ms: None,
            output_path: None,
        }
    }

    /// Regression: a failed pipeline segment must fail the ticket. Before
    /// pipefail, `exit 3 | cat` produced `exit: 0` and a successful ticket,
    /// so a failing `cargo clippy … | tail` looked like a pass.
    #[tokio::test]
    async fn failing_pipeline_fails_the_ticket() {
        let runner = ProcessLaneRunner::unbound();
        let result = runner.run(&ticket("exit 3 | cat")).await;
        assert_ne!(
            result.exit, 0,
            "a failed pipeline segment must not be reported as success: {result:?}"
        );
        assert_eq!(result.exit, 3);
    }

    #[tokio::test]
    async fn succeeding_pipeline_succeeds_the_ticket() {
        let runner = ProcessLaneRunner::unbound();
        let result = runner.run(&ticket("true | true")).await;
        assert_eq!(result.exit, 0, "{result:?}");
    }
}
