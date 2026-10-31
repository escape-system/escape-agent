//! `shell` tool: foreground command execution with combined, file-backed output.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

pub const DEFAULT_TIMEOUT_MS: u64 = 2 * 60 * 1_000;
pub const MAX_LINES: usize = 2_000;
pub const MAX_BYTES: usize = 50 * 1024;
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Default, Clone, Copy)]
pub struct ShellTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Posix,
    PowerShell,
    Cmd,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. Quote file paths containing spaces or special characters. Prefer dedicated tools over shell commands when possible. When output is large, the full result is saved to a file and a truncated preview is returned. Rely on automatic truncation unless filtering the output is more useful. Commands accept an optional timeout; a timeout of 0 disables it."
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
                    "description": "Timeout in milliseconds. Set to 0 to disable the timeout. Defaults to 120000 for foreground commands."
                },
                "background": {
                    "type": "boolean",
                    "description": "Run the command in the background and return immediately (useful for dev servers and long-running builds). Background execution is not available in phase 1."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)] // Foreground execution, capture and presentation are one flow.
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let command = util::required_string(&input, "command")?;
        let workdir_input = util::optional_string(&input, "workdir")?;
        let timeout_ms = util::optional_u64(&input, "timeout")?.unwrap_or(DEFAULT_TIMEOUT_MS);
        let background = util::optional_bool(&input, "background")?.unwrap_or(false);

        if background {
            return Err(ToolError::new(
                "Background shell execution is not supported in phase 1; run the command in the foreground or start a service.",
            ));
        }

        let workdir = match workdir_input {
            Some(value) => util::resolve_from(&ctx.cwd, &value),
            None => ctx.cwd.clone(),
        };
        match fs::metadata(&workdir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(ToolError::new(format!(
                    "Working directory is not a directory: {}",
                    workdir.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ToolError::new(format!(
                    "Working directory does not exist: {}",
                    workdir.display()
                )));
            }
            Err(error) => {
                return Err(ToolError::new(format!(
                    "Unable to access working directory {}: {error}",
                    workdir.display()
                )));
            }
        }

        let (shell, kind) = select_shell();
        let shell_id = format!("sh_{:012x}", util::unique() & 0xffff_ffff_ffff);
        let output_path = util::output_file("");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(&output_path)
            .map_err(|error| ToolError::new(format!("Unable to capture shell output: {error}")))?;
        let stdout = file
            .try_clone()
            .map_err(|error| ToolError::new(format!("Unable to capture shell output: {error}")))?;
        let stderr = file
            .try_clone()
            .map_err(|error| ToolError::new(format!("Unable to capture shell output: {error}")))?;
        drop(file);

        let mut process = Command::new(&shell);
        match kind {
            ShellKind::Cmd => {
                process.arg("/c").arg(&command);
            }
            ShellKind::PowerShell => {
                process
                    .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                    .arg(&command);
            }
            ShellKind::Posix => {
                process.arg("-c").arg(&command);
            }
        }
        process
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .env("TERM", "xterm-256color")
            .env("OPENCODE_TERMINAL", "1")
            .env("OPENCODE", "1")
            .env("AGENT", "1")
            .env("OPENCODE_SESSION_ID", &ctx.session_id)
            .kill_on_drop(true);
        if std::env::var_os("AI_AGENT").is_none() {
            process.env("AI_AGENT", "opencode");
        }
        #[cfg(unix)]
        process.process_group(0);

        let mut child = process.spawn().map_err(|error| {
            ToolError::new(format!("Unable to execute command: {command}: {error}"))
        })?;
        let pid = child.id().unwrap_or(0);

        let mut timed_out = false;
        let status = if timeout_ms == 0 {
            child.wait().await.map_err(|error| {
                ToolError::new(format!("Unable to execute command: {command}: {error}"))
            })?
        } else {
            let deadline = Instant::now().checked_add(Duration::from_millis(timeout_ms));
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {}
                    Err(error) => {
                        return Err(ToolError::new(format!(
                            "Unable to execute command: {command}: {error}"
                        )));
                    }
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    timed_out = true;
                    break kill_tree(&mut child, pid).await.map_err(|error| {
                        ToolError::new(format!("Unable to terminate timed out command: {error}"))
                    })?;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        let (text, truncated, total, shown) = capture_tail(&output_path)
            .map_err(|error| ToolError::new(format!("Unable to read shell output: {error}")))?;

        let exit = status.code();
        #[cfg(unix)]
        let signal = if timed_out {
            None
        } else {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(signal_name)
        };
        #[cfg(not(unix))]
        let signal: Option<&'static str> = None;

        let mut body = if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        };
        if truncated {
            let first = total.saturating_sub(shown) + 1;
            let _ = write!(
                body,
                "\n\n[showing lines {first}-{total} of {total}; full output saved to {}]",
                output_path.display()
            );
        }

        let mut parts = vec![body];
        if timed_out {
            parts.push(format!(
                "Command exceeded timeout of {timeout_ms} ms. Retry with a larger timeout if the command is expected to take longer."
            ));
        } else if let Some(signal) = signal {
            parts.push(format!("Killed by {signal}"));
        } else if let Some(code) = exit
            && code != 0
        {
            parts.push(format!("Exited with code {code}"));
        }

        let mut metadata = json!({
            "status": "completed",
            "truncated": truncated,
            "shellID": shell_id,
            "outputPath": output_path.display().to_string(),
        });
        if let Some(object) = metadata.as_object_mut() {
            if !timed_out {
                if let Some(code) = exit {
                    object.insert("exit".to_string(), json!(code));
                }
                if let Some(signal) = signal {
                    object.insert("signal".to_string(), json!(signal));
                }
            }
            if timed_out {
                object.insert("timeout".to_string(), json!(true));
            }
        }

        Ok(ToolResult::new(
            Some(parts.join("\n\n")),
            None,
            Some(metadata),
        ))
    }
}

fn capture_tail(path: &Path) -> io::Result<(String, bool, usize, usize)> {
    let length = fs::metadata(path)?.len();
    let mut file = File::open(path)?;
    let start = length.saturating_sub(MAX_BYTES as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let tail = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines: Vec<&str> = tail.split('\n').collect();
    if tail.ends_with('\n') {
        lines.pop();
    }
    let total = util::count_lines(path)?;
    let shown = lines.len().min(MAX_LINES);
    let truncated = length > MAX_BYTES as u64 || total > MAX_LINES;
    let text = if lines.len() > MAX_LINES {
        lines[lines.len() - MAX_LINES..].join("\n")
    } else {
        tail
    };
    Ok((text, truncated, total, shown))
}

async fn kill_tree(child: &mut Child, pid: u32) -> io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        send_signal(pid, "-TERM");
        let deadline = Instant::now() + KILL_GRACE;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        send_signal(pid, "-KILL");
        let _ = child.kill().await;
    }
    #[cfg(windows)]
    {
        let pid = pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        let _ = child.kill().await;
    }
    child.wait().await
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) {
    if pid <= 1 {
        return;
    }
    let _ = std::process::Command::new("kill")
        .arg(signal)
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
                let kind = shell_kind(&path);
                return (path, kind);
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

#[cfg(unix)]
fn shell_kind(_path: &Path) -> ShellKind {
    ShellKind::Posix
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
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
