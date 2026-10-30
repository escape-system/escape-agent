//! Lane integration: the process runner, the lane-aware `shell` tool and the
//! coordinator `lane` tool.
//!
//! [`ProcessLaneRunner`] executes tickets for [`oc_lanes::LaneService`]. The
//! service assigns tickets to lanes with semaphores; the runner only spawns the
//! configured shell, captures combined stdout/stderr into a temp file and
//! posts the file path back through [`RunnerResult::output_path`].
//!
//! [`ShellLaneTool`] replaces the plain `shell` tool when a lane service is
//! attached. Build/test commands are classified with [`oc_lanes::classify`] and
//! submitted as tickets; everything else delegates to
//! [`oc_tools::ShellTool`] unchanged.
//!
//! [`LaneTool`] is the coordinator surface (`queue`, `order`, `status`,
//! `cancel`, `notify`). Permission mapping belongs to the host: it authorises
//! calls to this tool under the `lane` action.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use oc_lanes::{
    BoxFuture, CancellationTokens, LaneError, LaneRunner, LaneService, NewTicket, RunnerResult,
    Ticket, TicketState, classify,
};
use oc_store::{Delivery, NewInbox};
use oc_tools::util;
use oc_tools::{ShellTool, Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::MessageId;
use serde_json::{Value, json};
use tokio::process::Command;

/// Default lane wait (5 minutes) when the caller does not pass `timeout`.
pub const DEFAULT_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
/// Maximum number of trailing lines returned by the lane tools.
pub const MAX_LINES: usize = 2_000;
/// Maximum number of trailing bytes returned by the lane tools.
pub const MAX_BYTES: usize = 50 * 1024;
/// Longest timeout a caller can express; `timeout: 0` means this.
const NO_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const LANE_DIR: &str = "opencode-lanes";

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
            kind.apply(&mut command, &ticket.command);
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
    if pid == 0 {
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
    PowerShell,
    Cmd,
}

impl ShellKind {
    fn apply(self, command: &mut Command, source: &str) {
        match self {
            Self::Cmd => {
                command.arg("/c").arg(source);
            }
            Self::PowerShell => {
                command
                    .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                    .arg(source);
            }
            Self::Posix => {
                command.arg("-c").arg(source);
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

// ---------------------------------------------------------------------------
// Output capture
// ---------------------------------------------------------------------------

struct Tail {
    text: String,
    truncated: bool,
}

/// Reads at most [`MAX_BYTES`] from the end of `path` and keeps at most
/// [`MAX_LINES`] trailing lines.
fn read_tail(path: &Path) -> io::Result<Tail> {
    let length = fs::metadata(path)?.len();
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(length.saturating_sub(MAX_BYTES as u64)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let tail = String::from_utf8_lossy(&bytes).into_owned();
    let mut lines: Vec<&str> = tail.split('\n').collect();
    if tail.ends_with('\n') {
        lines.pop();
    }
    let total = util::count_lines(path)?;
    let truncated = length > MAX_BYTES as u64 || total > MAX_LINES;
    let text = if lines.len() > MAX_LINES {
        lines[lines.len() - MAX_LINES..].join("\n")
    } else {
        tail
    };
    Ok(Tail { text, truncated })
}

fn tail_body(tail: Tail) -> String {
    if tail.text.is_empty() {
        "(no output)".to_owned()
    } else if tail.truncated {
        format!(
            "{}\n\n[truncated to the last {} lines / {} KiB]",
            tail.text,
            MAX_LINES,
            MAX_BYTES / 1024
        )
    } else {
        tail.text
    }
}

fn status_name(state: &TicketState) -> &'static str {
    match state {
        TicketState::Queued => "queued",
        TicketState::Running => "running",
        TicketState::Succeeded { .. } => "succeeded",
        TicketState::Failed { .. } => "failed",
        TicketState::Cancelled => "cancelled",
    }
}

fn status_exit(state: &TicketState) -> Option<i32> {
    match state {
        TicketState::Succeeded { exit } | TicketState::Failed { exit, .. } => Some(*exit),
        TicketState::Queued | TicketState::Running | TicketState::Cancelled => None,
    }
}

fn optional_i64(input: &Value, key: &str) -> Result<Option<i64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_i64().map(Some).ok_or_else(|| {
            ToolError::new(format!("Invalid arguments: `{key}` must be an integer"))
        }),
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be an integer"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Shell tool with lane routing
// ---------------------------------------------------------------------------

/// Lane-aware replacement for [`oc_tools::ShellTool`].
///
/// The registry key stays `shell`. Classified build/test commands become lane
/// tickets; unclassified commands and explicit `background` requests delegate
/// to the plain shell tool.
#[derive(Debug, Clone)]
pub struct ShellLaneTool {
    runtime: crate::Runtime,
    shell: ShellTool,
}

impl ShellLaneTool {
    /// Builds the tool over a runtime that may have a lane service attached.
    #[must_use]
    pub fn new(runtime: crate::Runtime) -> Self {
        Self {
            runtime,
            shell: ShellTool,
        }
    }
}

#[async_trait]
impl Tool for ShellLaneTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command and return its output. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. Build and test commands are serialized through build lanes and wait for their turn before returning. When output is large, the full result is saved to a file and a truncated preview is returned. Commands accept an optional timeout; a timeout of 0 disables it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command string to execute"
                },
                "workdir": {
                    "type": "string",
                    "description": "Working directory to execute the command in. Defaults to the current working directory. When possible, avoid changing directories in the command and set the working directory here instead."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 300000 for lane-routed commands."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). Background execution is not available in phase 1."
                },
                "lane": {
                    "type": "string",
                    "description": "Explicit lane override. When omitted, build/test commands are classified into cargo/node/python/… lanes automatically."
                },
                "priority": {
                    "type": "integer",
                    "description": "Queue priority inside the lane. Higher values run first; equal priorities stay FIFO."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)] // Delegate/enqueue/wait/report are one flow.
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let command = util::required_string(&input, "command")?;
        let workdir_input = util::optional_string(&input, "workdir")?;
        let timeout_ms = util::optional_u64(&input, "timeout")?.unwrap_or(DEFAULT_TIMEOUT_MS);
        let explicit_lane =
            util::optional_string(&input, "lane")?.filter(|lane| !lane.trim().is_empty());
        let priority = optional_i64(&input, "priority")?.unwrap_or(0);
        let background = util::optional_bool(&input, "background")?.unwrap_or(false);

        let cwd = match workdir_input {
            Some(value) => util::resolve_from(&ctx.cwd, &value),
            None => ctx.cwd.clone(),
        };
        if background {
            return self.shell.execute(ctx, input).await;
        }

        let request = explicit_lane.map_or_else(
            || classify(&command, &cwd),
            |lane| {
                Some(oc_lanes::LaneRequest {
                    lane,
                    command: command.clone(),
                })
            },
        );
        let Some(request) = request else {
            return self.shell.execute(ctx, input).await;
        };
        let Some(lanes) = self.runtime.lanes() else {
            return self.shell.execute(ctx, input).await;
        };

        let ticket_id = lanes
            .enqueue(
                request.lane.clone(),
                NewTicket {
                    command: request.command,
                    cwd,
                    priority: clamp_priority(priority),
                    owner_session: Some(ctx.session_id.clone()),
                    label: None,
                },
            )
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;

        let timeout = if timeout_ms == 0 {
            NO_TIMEOUT
        } else {
            Duration::from_millis(timeout_ms).min(NO_TIMEOUT)
        };
        let ticket = match lanes.wait(&ticket_id, timeout).await {
            Ok(ticket) => ticket,
            Err(LaneError::Timeout { .. }) => {
                let _ = lanes.cancel(&ticket_id).await;
                let metadata = json!({
                    "lane": request.lane,
                    "ticket": ticket_id,
                    "status": "timeout",
                });
                return Err(ToolError::new(format!(
                    "Lane ticket {ticket_id} did not finish within {timeout_ms} ms; it was cancelled. Retry with a larger timeout if the command is expected to take longer."
                ))
                .with_metadata(metadata));
            }
            Err(error) => return Err(ToolError::new(error.to_string())),
        };

        let status = status_name(&ticket.state);
        let mut metadata = json!({
            "lane": ticket.lane,
            "ticket": ticket.id,
            "status": status,
            "truncated": false,
        });
        if let Some(exit) = status_exit(&ticket.state)
            && let Some(object) = metadata.as_object_mut()
        {
            object.insert("exit".to_owned(), json!(exit));
        }
        let path = ticket.output_path.clone();
        if let Some(path) = &path
            && let Some(object) = metadata.as_object_mut()
        {
            object.insert(
                "outputPath".to_owned(),
                Value::String(path.display().to_string()),
            );
        }
        let tail = match path.as_deref().map(read_tail) {
            Some(Ok(tail)) => tail,
            Some(Err(error)) => {
                return Err(ToolError::new(format!(
                    "Unable to read lane output for {}: {error}",
                    ticket.id
                ))
                .with_metadata(metadata));
            }
            None => Tail {
                text: String::new(),
                truncated: false,
            },
        };
        if let Some(object) = metadata.as_object_mut() {
            object.insert("truncated".to_owned(), json!(tail.truncated));
        }
        let mut body = tail_body(tail);

        match &ticket.state {
            TicketState::Cancelled => {
                return Err(
                    ToolError::new(format!("Lane ticket {} was cancelled", ticket.id))
                        .with_metadata(metadata),
                );
            }
            TicketState::Failed { exit, message } => {
                let _ = write!(body, "\n\nExited with code {exit}: {message}");
            }
            _ => {}
        }

        Ok(ToolResult::new(Some(body), None, Some(metadata)))
    }
}

fn clamp_priority(priority: i64) -> i32 {
    i32::try_from(priority).unwrap_or(if priority < 0 { i32::MIN } else { i32::MAX })
}

// ---------------------------------------------------------------------------
// Coordinator lane tool
// ---------------------------------------------------------------------------

/// Coordinator tool for ordering, cancelling and notifying lane work.
#[derive(Debug, Clone)]
pub struct LaneTool {
    runtime: crate::Runtime,
}

impl LaneTool {
    /// Builds the tool over a runtime.
    #[must_use]
    pub fn new(runtime: crate::Runtime) -> Self {
        Self { runtime }
    }

    fn lanes(&self) -> Result<std::sync::Arc<LaneService>, ToolError> {
        self.runtime
            .lanes()
            .ok_or_else(|| ToolError::new("The lane service is not available."))
    }
}

#[async_trait]
impl Tool for LaneTool {
    fn name(&self) -> &'static str {
        "lane"
    }

    fn description(&self) -> &'static str {
        "Inspect and steer build lanes. `queue` submits a command as a ticket, `order` changes a queued ticket's priority, `status` returns lane or ticket state, `cancel` stops a ticket, and `notify` sends a message to another session."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["queue", "order", "status", "cancel", "notify"],
                    "description": "Lane operation to perform."
                },
                "lane": {
                    "type": "string",
                    "description": "Lane name for `queue`; defaults to the classifier result, then `default`. For `status`, restricts the snapshot to one lane."
                },
                "command": {
                    "type": "string",
                    "description": "Command string for `queue`."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for `queue`. Defaults to the current working directory."
                },
                "priority": {
                    "type": "integer",
                    "description": "Queue priority. Higher values run first; equal priorities stay FIFO."
                },
                "owner": {
                    "type": "string",
                    "description": "Owning session id. Defaults to the calling session."
                },
                "ticket": {
                    "type": "string",
                    "description": "Ticket id for `order`, `cancel` or `status`."
                },
                "session_id": {
                    "type": "string",
                    "description": "Target session id for `notify`."
                },
                "text": {
                    "type": "string",
                    "description": "Message text for `notify`."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let action = util::required_string(&input, "action")?;
        match action.as_str() {
            "queue" => self.queue(ctx, &input).await,
            "order" => self.order(&input).await,
            "status" => self.status(&input).await,
            "cancel" => self.cancel(&input).await,
            "notify" => self.notify(&input).await,
            other => Err(ToolError::new(format!(
                "Invalid arguments: unknown lane action `{other}`; expected queue, order, status, cancel or notify"
            ))),
        }
    }
}

impl LaneTool {
    async fn queue(&self, ctx: &ToolContext, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let command = util::required_string(input, "command")?;
        let cwd = match util::optional_string(input, "cwd")? {
            Some(value) => util::resolve_from(&ctx.cwd, &value),
            None => ctx.cwd.clone(),
        };
        let priority = clamp_priority(optional_i64(input, "priority")?.unwrap_or(0));
        let owner = util::optional_string(input, "owner")?.or_else(|| Some(ctx.session_id.clone()));
        let lane =
            match util::optional_string(input, "lane")?.filter(|lane| !lane.trim().is_empty()) {
                Some(lane) => lane,
                None => classify(&command, &cwd).map_or_else(|| "default".to_owned(), |r| r.lane),
            };
        let id = lanes
            .enqueue(
                lane.clone(),
                NewTicket {
                    command,
                    cwd,
                    priority,
                    owner_session: owner,
                    label: None,
                },
            )
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let ticket = lanes.ticket(&id).await;
        let output = json!({
            "ticket": id,
            "lane": lane,
            "ticketDetails": ticket,
        });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn order(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::required_string(input, "ticket")?;
        let priority = optional_i64(input, "priority")?.ok_or_else(|| {
            ToolError::new("Invalid arguments: missing required argument `priority`")
        })?;
        lanes
            .reorder(&ticket, clamp_priority(priority))
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({ "ticket": ticket, "priority": priority });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn status(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::optional_string(input, "ticket")?.filter(|value| !value.is_empty());
        let lane = util::optional_string(input, "lane")?.filter(|value| !value.is_empty());
        let output = if let Some(id) = ticket {
            let ticket = lanes.ticket(&id).await;
            json!({ "ticket": ticket })
        } else {
            let statuses = lanes.status(lane.as_deref()).await;
            json!({ "lanes": statuses })
        };
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn cancel(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let lanes = self.lanes()?;
        let ticket = util::required_string(input, "ticket")?;
        let cancelled = lanes
            .cancel(&ticket)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({ "ticket": ticket, "cancelled": cancelled });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }

    async fn notify(&self, input: &Value) -> Result<ToolResult, ToolError> {
        let session_id = util::required_string(input, "session_id")?;
        let text = util::required_string(input, "text")?;
        if text.trim().is_empty() {
            return Err(ToolError::new(
                "Invalid arguments: `text` must not be empty",
            ));
        }
        let message_id = MessageId::new().into_inner();
        let store = self.runtime.inner.config.store.clone();
        store
            .enqueue(NewInbox {
                id: message_id.clone(),
                session_id: session_id.clone(),
                kind: "synthetic".to_owned(),
                payload: json!({ "text": text, "peer": true }),
                delivery: Delivery::Steer,
            })
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        self.runtime
            .notify(&session_id)
            .await
            .map_err(|error| ToolError::new(error.to_string()))?;
        let output = json!({ "sessionID": session_id, "messageID": message_id });
        Ok(ToolResult::new(
            Some(serde_json::to_string_pretty(&output).unwrap_or_else(|_| output.to_string())),
            None,
            Some(output),
        ))
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Registry factory for the lane-aware tools.
#[derive(Debug, Clone)]
pub struct LaneTools {
    runtime: crate::Runtime,
}

impl LaneTools {
    /// Tools bound to one runtime.
    #[must_use]
    pub fn new(runtime: crate::Runtime) -> Self {
        Self { runtime }
    }

    /// Registry containing the `shell` replacement and the `lane` tool.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(ShellLaneTool::new(
            self.runtime.clone(),
        )));
        registry.register(std::sync::Arc::new(LaneTool::new(self.runtime.clone())));
        registry
    }
}
