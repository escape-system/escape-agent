//! Persistent `ripwire <root> [--scip=...] --mcp` child: spawn/initialize,
//! JSON-RPC request/response pairing and tool invocation.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::pool::{acquire_slot, mark_cli_only};
use super::wire::{extract_mcp_text, truncate_snippet};
use super::{McpCallError, McpToolOutput};

/// Maximum stderr bytes buffered per daemon before oldest content is discarded.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Environment variables passed through to `ripwire --mcp`, matching CLI execution.
const ALLOWED_ENV_VARS: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR"];

#[derive(Debug)]
pub(super) struct McpDaemon {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_buf: Arc<StdMutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    next_id: u64,
}

impl Drop for McpDaemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}

impl McpDaemon {
    async fn spawn_and_init(
        binary: &Path,
        root: &Path,
        scip_path: Option<&Path>,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<Self, McpCallError> {
        let mut cmd = Command::new(binary);
        cmd.arg(root);
        if let Some(scip) = scip_path {
            cmd.arg(format!("--scip={}", scip.display()));
        }
        cmd.arg("--mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .kill_on_drop(true);
        for key in ALLOWED_ENV_VARS {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
        }

        let mut child = spawn_retrying_busy_executable(&mut cmd, binary, cancel).await?;

        let stdin = child.stdin.take().ok_or_else(|| McpCallError::Io {
            message: "ripwire --mcp missing stdin pipe".to_string(),
            stderr: Vec::new(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpCallError::Io {
            message: "ripwire --mcp missing stdout pipe".to_string(),
            stderr: Vec::new(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpCallError::Io {
            message: "ripwire --mcp missing stderr pipe".to_string(),
            stderr: Vec::new(),
        })?;

        let stderr_buf = Arc::new(StdMutex::new(Vec::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buf);
        let stderr_task = tokio::spawn(async move {
            drain_stderr_pipe(stderr, stderr_buf_clone).await;
        });

        let mut daemon = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_buf,
            stderr_task,
            next_id: 1,
        };

        daemon.initialize_session(cancel, timeout).await?;

        // Drain startup diagnostics before serving real tool calls.
        let _ = daemon.take_stderr();
        Ok(daemon)
    }

    async fn initialize_session(
        &mut self,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<(), McpCallError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let init_id = self.take_id();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "opencode-rust",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        });

        if let Err(err) = self.write_json(&init_req).await {
            let stderr = self.take_stderr();
            return Err(match err {
                McpCallError::Io { .. } => McpCallError::CliOnly { stderr },
                other => other,
            });
        }

        match self
            .read_response_for_id(init_id, cancel, deadline, timeout, true)
            .await
        {
            Ok(val) if val.get("result").is_some() => {}
            Ok(_) => {
                let stderr = self.take_stderr();
                return Err(McpCallError::CliOnly { stderr });
            }
            Err(err) => return Err(err),
        }

        let initialized_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        self.write_json(&initialized_notif).await?;

        // Pre-read full legend dictionary once per persistent session so
        // subsequent verb responses omit repeated XML glossary bloat.
        let dict_id = self.take_id();
        let dict_req = json!({
            "jsonrpc": "2.0",
            "id": dict_id,
            "method": "resources/read",
            "params": {
                "uri": "ripwire://legend-dict/full"
            }
        });
        if self.write_json(&dict_req).await.is_ok()
            && let Err(err @ (McpCallError::Timeout { .. } | McpCallError::Cancelled)) = self
                .read_response_for_id(dict_id, cancel, deadline, timeout, false)
                .await
        {
            return Err(err);
        }

        Ok(())
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn take_stderr(&self) -> Vec<u8> {
        let mut guard = self
            .stderr_buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    }

    async fn write_json(&mut self, msg: &Value) -> Result<(), McpCallError> {
        let mut line = serde_json::to_vec(msg).map_err(|err| McpCallError::Io {
            message: format!("failed to serialize MCP request: {err}"),
            stderr: self.take_stderr(),
        })?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|err| McpCallError::Io {
                message: format!("failed writing to ripwire --mcp stdin: {err}"),
                stderr: self.take_stderr(),
            })?;
        self.stdin.flush().await.map_err(|err| McpCallError::Io {
            message: format!("failed flushing ripwire --mcp stdin: {err}"),
            stderr: self.take_stderr(),
        })?;
        Ok(())
    }

    async fn read_response_for_id(
        &mut self,
        expected_id: u64,
        cancel: &CancellationToken,
        deadline: tokio::time::Instant,
        timeout: Duration,
        allow_cli_fallback_on_non_json: bool,
    ) -> Result<Value, McpCallError> {
        let mut line = String::new();
        loop {
            line.clear();
            let read_res = tokio::select! {
                () = cancel.cancelled() => {
                    return Err(McpCallError::Cancelled);
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Err(McpCallError::Timeout { timeout });
                }
                res = self.stdout.read_line(&mut line) => res,
            };

            let bytes_read = match read_res {
                Ok(n) => n,
                Err(err) => {
                    let stderr = self.take_stderr();
                    return Err(if allow_cli_fallback_on_non_json {
                        McpCallError::CliOnly { stderr }
                    } else {
                        McpCallError::Io {
                            message: format!("failed reading ripwire --mcp stdout: {err}"),
                            stderr,
                        }
                    });
                }
            };

            if bytes_read == 0 {
                // Brief grace yield lets the stderr reader finish flushing.
                tokio::task::yield_now().await;
                let stderr = self.take_stderr();
                return Err(if allow_cli_fallback_on_non_json {
                    McpCallError::CliOnly { stderr }
                } else {
                    McpCallError::Io {
                        message: "ripwire --mcp closed stdout unexpectedly".to_string(),
                        stderr,
                    }
                });
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
                let stderr = self.take_stderr();
                return Err(if allow_cli_fallback_on_non_json {
                    McpCallError::CliOnly { stderr }
                } else {
                    McpCallError::Io {
                        message: format!(
                            "invalid JSON-RPC line from ripwire --mcp: {}",
                            truncate_snippet(trimmed, 120)
                        ),
                        stderr,
                    }
                });
            };

            let Some(msg_id) = parsed.get("id").and_then(Value::as_u64) else {
                // Skip server notifications that carry no request id.
                continue;
            };
            if msg_id == expected_id {
                return Ok(parsed);
            }
        }
    }

    async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: &Value,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<McpToolOutput, McpCallError> {
        let req_id = self.take_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            }
        });
        let deadline = tokio::time::Instant::now() + timeout;
        self.write_json(&req).await?;
        let resp = self
            .read_response_for_id(req_id, cancel, deadline, timeout, false)
            .await?;
        let stderr = self.take_stderr();

        if let Some(err_obj) = resp.get("error") {
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| err_obj.to_string(), ToString::to_string);
            return Err(McpCallError::Tool { message, stderr });
        }

        let Some(result) = resp.get("result") else {
            return Err(McpCallError::Io {
                message: "ripwire --mcp response missing result field".to_string(),
                stderr,
            });
        };

        let text = extract_mcp_text(result);
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(McpCallError::Tool {
                message: if text.is_empty() {
                    "ripwire --mcp reported an error".to_string()
                } else {
                    text
                },
                stderr,
            });
        }

        Ok(McpToolOutput { text, stderr })
    }
}

/// Spawns the MCP child, retrying a transient `ETXTBSY`.
///
/// `execve` fails with `ETXTBSY` ("Text file busy") while any process still
/// holds the target open for writing. That is not a hypothetical: `ripwire` is
/// rebuilt by the release/test pipeline, and a freshly written or freshly
/// copied executable can be briefly undroppable while the writer's descriptor
/// is open. It is also reproducible under load in this workspace's own test
/// suite (observed as `failed to spawn … --mcp: Text file busy (os error 26)`
/// in 3 of 40 runs).
///
/// The condition is momentary, so retrying briefly is the correct response; a
/// binary that is *genuinely* not runnable fails fast with a different errno
/// (`EACCES`, `ENOEXEC`, `ENOENT`) and is surfaced unchanged as `Io`, exactly as
/// before. A deadlock is impossible: the loop is bounded and cancellation is
/// honoured on every iteration.
async fn spawn_retrying_busy_executable(
    cmd: &mut Command,
    binary: &Path,
    cancel: &CancellationToken,
) -> Result<Child, McpCallError> {
    /// Total attempts for a busy executable, including the first.
    const BUSY_EXEC_ATTEMPTS: u32 = 40;
    /// Backoff between `ETXTBSY` attempts. Short enough that a healthy spawn is
    /// imperceptibly delayed, long enough for a finishing writer to close.
    const BUSY_EXEC_BACKOFF: Duration = Duration::from_millis(5);

    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(err)
                if err.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempt < BUSY_EXEC_ATTEMPTS =>
            {
                tokio::select! {
                    () = cancel.cancelled() => return Err(McpCallError::Cancelled),
                    () = tokio::time::sleep(BUSY_EXEC_BACKOFF) => {}
                }
            }
            Err(err) => {
                return Err(McpCallError::Io {
                    message: format!("failed to spawn {} --mcp: {err}", binary.display()),
                    stderr: Vec::new(),
                });
            }
        }
    }
}

async fn drain_stderr_pipe(mut pipe: tokio::process::ChildStderr, buf: Arc<StdMutex<Vec<u8>>>) {
    let mut chunk = [0_u8; 4096];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut guard = buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let available = MAX_STDERR_BYTES.saturating_sub(guard.len());
                if available == 0 {
                    let drop_count = n.min(guard.len());
                    guard.drain(..drop_count);
                }
                let take = n.min(MAX_STDERR_BYTES.saturating_sub(guard.len()));
                if take > 0 {
                    guard.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
}

/// Execute an MCP tool call on a persistent `ripwire <root> --mcp` daemon.
#[cfg(test)]
pub(in crate::code_map) async fn call_mcp_tool(
    binary: &Path,
    root: &Path,
    cancel: &CancellationToken,
    timeout: Duration,
    tool_name: &str,
    arguments: &Value,
) -> Result<McpToolOutput, McpCallError> {
    call_mcp_tool_with_scip(binary, root, None, cancel, timeout, tool_name, arguments).await
}

/// Execute an MCP tool call on a persistent `ripwire <root> [--scip=...] --mcp` daemon.
pub(crate) async fn call_mcp_tool_with_scip(
    binary: &Path,
    root: &Path,
    scip_path: Option<&Path>,
    cancel: &CancellationToken,
    timeout: Duration,
    tool_name: &str,
    arguments: &Value,
) -> Result<McpToolOutput, McpCallError> {
    let (fingerprint, slot) = match acquire_slot(binary, root, scip_path) {
        Ok(pair) => pair,
        Err(stderr) => return Err(McpCallError::CliOnly { stderr }),
    };

    let mut guard = tokio::select! {
        () = cancel.cancelled() => return Err(McpCallError::Cancelled),
        g = slot.daemon.lock() => g,
    };

    let was_already_warm = guard.is_some();
    if guard.is_none() {
        match McpDaemon::spawn_and_init(binary, root, scip_path, cancel, timeout).await {
            Ok(daemon) => {
                *guard = Some(daemon);
            }
            Err(McpCallError::CliOnly { stderr }) => {
                mark_cli_only(fingerprint, root, scip_path);
                return Err(McpCallError::CliOnly { stderr });
            }
            Err(err) => return Err(err),
        }
    }

    let first_attempt = guard
        .as_mut()
        .expect("daemon initialized")
        .call_tool(tool_name, arguments, cancel, timeout)
        .await;

    match first_attempt {
        Ok(out) => {
            slot.touch();
            Ok(out)
        }
        Err(err @ (McpCallError::Timeout { .. } | McpCallError::Cancelled)) => {
            // Drop and kill daemon on timeout or cancel so pipe never desyncs.
            guard.take();
            Err(err)
        }
        Err(McpCallError::Io { .. }) if was_already_warm => {
            // Broken pipe / restarted child: respawn once transparently.
            guard.take();
            let mut fresh =
                match McpDaemon::spawn_and_init(binary, root, scip_path, cancel, timeout).await {
                    Ok(daemon) => daemon,
                    Err(McpCallError::CliOnly { stderr }) => {
                        mark_cli_only(fingerprint, root, scip_path);
                        return Err(McpCallError::CliOnly { stderr });
                    }
                    Err(err) => return Err(err),
                };
            let retry_res = fresh.call_tool(tool_name, arguments, cancel, timeout).await;
            match retry_res {
                Ok(out) => {
                    *guard = Some(fresh);
                    slot.touch();
                    Ok(out)
                }
                Err(err) => {
                    if matches!(err, McpCallError::Timeout { .. } | McpCallError::Cancelled) {
                        drop(fresh);
                    } else {
                        *guard = Some(fresh);
                    }
                    Err(err)
                }
            }
        }
        Err(err) => {
            if matches!(err, McpCallError::Io { .. }) {
                guard.take();
            } else {
                slot.touch();
            }
            Err(err)
        }
    }
}
