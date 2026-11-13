//! Lazy, bounded persistent `ripwire --mcp` daemon pool.
//!
//! Spawning `ripwire <root>` per invocation reloads the workspace graph from
//! disk (180–560 ms) and repeats 1–2 KB of XML legend definitions per response.
//! Keeping up to `MAX_DAEMONS` warm `ripwire --mcp` stdio children after first
//! use and reading `ripwire://legend-dict/full` once per session eliminates both
//! costs while keeping boot idle RSS at zero and automatically reflecting new
//! on-disk edits on subsequent queries.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Maximum concurrent warm `ripwire --mcp` daemons across workspaces.
const MAX_DAEMONS: usize = 2;

/// Idle eviction threshold for unused `ripwire --mcp` daemons.
const DAEMON_IDLE_TTL: Duration = Duration::from_secs(300);

/// Periodic check interval for the lazy idle reaper task.
const REAPER_INTERVAL: Duration = Duration::from_secs(60);

/// Maximum stderr bytes buffered per daemon before oldest content is discarded.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Environment variables passed through to `ripwire --mcp`, matching CLI execution.
const ALLOWED_ENV_VARS: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR"];

#[derive(Debug, Clone)]
pub(crate) struct McpToolOutput {
    pub(crate) text: String,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum McpCallError {
    /// Binary exited or emitted non-JSON-RPC on initialize; fall back to CLI.
    CliOnly { stderr: Vec<u8> },
    /// Hard wall-clock deadline expired; must not fall back silently.
    Timeout { timeout: Duration },
    /// Caller cancelled execution via `ToolContext.cancel`.
    Cancelled,
    /// Persistent MCP daemon returned an explicit JSON-RPC or tool-level error.
    Tool { message: String, stderr: Vec<u8> },
    /// Process spawn, broken pipe after retry, or protocol framing error.
    Io { message: String, stderr: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BinaryFingerprint {
    path: PathBuf,
    len: u64,
    modified_nanos: Option<u128>,
}

impl BinaryFingerprint {
    fn of(path: &Path) -> Self {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match fs::metadata(&canonical) {
            Ok(meta) => {
                let modified_nanos = meta
                    .modified()
                    .ok()
                    .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
                    .map(|dur| dur.as_nanos());
                Self {
                    path: canonical,
                    len: meta.len(),
                    modified_nanos,
                }
            }
            Err(_) => Self {
                path: canonical,
                len: 0,
                modified_nanos: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PoolKey {
    runtime_id: tokio::runtime::Id,
    binary: BinaryFingerprint,
    root: PathBuf,
    scip: Option<BinaryFingerprint>,
}

#[derive(Debug)]
struct PoolSlot {
    daemon: AsyncMutex<Option<McpDaemon>>,
    last_used: StdMutex<Instant>,
}

impl PoolSlot {
    fn new() -> Self {
        Self {
            daemon: AsyncMutex::new(None),
            last_used: StdMutex::new(Instant::now()),
        }
    }

    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }

    fn idle_elapsed(&self, now: Instant) -> Duration {
        let last = *self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        now.saturating_duration_since(last)
    }
}

#[derive(Debug, Default)]
struct PoolState {
    slots: HashMap<PoolKey, Arc<PoolSlot>>,
    cli_only_binaries: HashSet<BinaryFingerprint>,
    reaper_runtimes: HashSet<tokio::runtime::Id>,
}

fn global_pool() -> &'static StdMutex<PoolState> {
    static POOL: OnceLock<StdMutex<PoolState>> = OnceLock::new();
    POOL.get_or_init(|| StdMutex::new(PoolState::default()))
}

#[derive(Debug)]
struct McpDaemon {
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

        let mut child = cmd.spawn().map_err(|err| McpCallError::Io {
            message: format!("failed to spawn {} --mcp: {err}", binary.display()),
            stderr: Vec::new(),
        })?;

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

fn extract_mcp_text(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return result.as_str().map(ToString::to_string).unwrap_or_default();
    };
    let mut parts = Vec::new();
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            parts.push(text);
        }
    }
    parts.join("\n")
}

fn truncate_snippet(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn canonical_root(root: &Path) -> PathBuf {
    fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn ensure_reaper_for_runtime(runtime: &tokio::runtime::Handle, pool: &mut PoolState) {
    let runtime_id = runtime.id();
    if pool.reaper_runtimes.contains(&runtime_id) {
        return;
    }
    pool.reaper_runtimes.insert(runtime_id);
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(REAPER_INTERVAL).await;
            let now = Instant::now();
            let to_drop = {
                let mut state = global_pool()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut evicted = Vec::new();
                state.slots.retain(|key, slot| {
                    if key.runtime_id != runtime_id {
                        return true;
                    }
                    if slot.idle_elapsed(now) >= DAEMON_IDLE_TTL {
                        // Only evict idle slots whose daemon lock is currently free.
                        if let Ok(mut guard) = slot.daemon.try_lock() {
                            if let Some(daemon) = guard.take() {
                                evicted.push(daemon);
                            }
                            return false;
                        }
                    }
                    true
                });
                let still_has_entries = state.slots.keys().any(|key| key.runtime_id == runtime_id);
                if !still_has_entries {
                    state.reaper_runtimes.remove(&runtime_id);
                }
                (evicted, !still_has_entries)
            };
            drop(to_drop.0);
            if to_drop.1 {
                break;
            }
        }
    });
}

fn acquire_slot(
    binary: &Path,
    root: &Path,
    scip_path: Option<&Path>,
) -> Result<(BinaryFingerprint, Arc<PoolSlot>), Vec<u8>> {
    let fingerprint = BinaryFingerprint::of(binary);
    let runtime = tokio::runtime::Handle::current();
    let key = PoolKey {
        runtime_id: runtime.id(),
        binary: fingerprint.clone(),
        root: canonical_root(root),
        scip: scip_path.map(BinaryFingerprint::of),
    };

    let evicted_slots: Vec<Arc<PoolSlot>>;
    let slot = {
        let mut state = global_pool()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cli_only_binaries.contains(&fingerprint) {
            return Err(Vec::new());
        }

        let now = Instant::now();
        let mut removed = Vec::new();
        // Reclaim stale slots idle past TTL whenever capacity is checked.
        state.slots.retain(|existing_key, existing_slot| {
            if existing_key != &key && existing_slot.idle_elapsed(now) >= DAEMON_IDLE_TTL {
                removed.push(Arc::clone(existing_slot));
                return false;
            }
            true
        });

        let slot = if let Some(existing) = state.slots.get(&key) {
            Arc::clone(existing)
        } else {
            // Bound active warm daemons per runtime to MAX_DAEMONS using LRU eviction,
            // never evicting a slot that is currently executing a call.
            loop {
                let same_runtime_count = state
                    .slots
                    .keys()
                    .filter(|k| k.runtime_id == key.runtime_id)
                    .count();
                if same_runtime_count < MAX_DAEMONS {
                    break;
                }
                let Some(oldest_key) = state
                    .slots
                    .iter()
                    .filter(|(k, s)| k.runtime_id == key.runtime_id && s.daemon.try_lock().is_ok())
                    .min_by_key(|(_, s)| {
                        *s.last_used
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    })
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some(evicted) = state.slots.remove(&oldest_key) {
                    removed.push(evicted);
                }
            }
            // Also keep total slots bounded across exited test runtimes.
            while state.slots.len() >= 16 {
                let Some(oldest_key) = state
                    .slots
                    .iter()
                    .filter(|(k, s)| k.runtime_id != key.runtime_id && s.daemon.try_lock().is_ok())
                    .min_by_key(|(_, s)| {
                        *s.last_used
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                    })
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                if let Some(evicted) = state.slots.remove(&oldest_key) {
                    removed.push(evicted);
                }
            }
            let created = Arc::new(PoolSlot::new());
            state.slots.insert(key, Arc::clone(&created));
            created
        };

        ensure_reaper_for_runtime(&runtime, &mut state);
        evicted_slots = removed;
        slot
    };

    for evicted in evicted_slots {
        if let Ok(mut guard) = evicted.daemon.try_lock() {
            guard.take();
        }
    }

    slot.touch();
    Ok((fingerprint, slot))
}

fn mark_cli_only(fingerprint: BinaryFingerprint, root: &Path, scip_path: Option<&Path>) {
    let runtime_id = tokio::runtime::Handle::current().id();
    let key = PoolKey {
        runtime_id,
        binary: fingerprint.clone(),
        root: canonical_root(root),
        scip: scip_path.map(BinaryFingerprint::of),
    };
    let mut state = global_pool()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.cli_only_binaries.len() >= 16 {
        state.cli_only_binaries.clear();
    }
    state.cli_only_binaries.insert(fingerprint);
    state.slots.remove(&key);
}

/// Execute an MCP tool call on a persistent `ripwire <root> --mcp` daemon.
#[cfg(test)]
pub(super) async fn call_mcp_tool(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn fake_mcp_daemon_reads_legend_once_and_reuses_process() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let counter_file = dir.path().join("spawns.log");
        let methods_file = dir.path().join("methods.log");
        let script = write_script(
            dir.path(),
            "fake-ripwire-mcp",
            &format!(
                r#"#!/usr/bin/env python3
import json, sys

with open({counter:?}, "a") as f:
    f.write("spawn\n")

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    with open({methods:?}, "a") as f:
        f.write(str(method) + "\n")
    rid = msg.get("id")
    if method == "initialize":
        resp = {{"jsonrpc":"2.0","id":rid,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}}}
        print(json.dumps(resp), flush=True)
    elif method == "resources/read":
        resp = {{"jsonrpc":"2.0","id":rid,"result":{{"contents":[{{"uri":"ripwire://legend-dict/full","text":"dict"}}]}} }}
        print(json.dumps(resp), flush=True)
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"]["arguments"]
        resp = {{"jsonrpc":"2.0","id":rid,"result":{{"content":[{{"type":"text","text":f"ok:{{name}}:{{args.get('symbol','')}}"}}],"isError":False}}}}
        print(json.dumps(resp), flush=True)
"#,
                counter = counter_file.to_string_lossy().into_owned(),
                methods = methods_file.to_string_lossy().into_owned(),
            ),
        );

        let cancel = CancellationToken::new();
        let timeout = Duration::from_secs(5);
        let first = call_mcp_tool(
            &script,
            &root,
            &cancel,
            timeout,
            "find_symbol",
            &json!({ "symbol": "Alpha" }),
        )
        .await
        .unwrap();
        assert_eq!(first.text, "ok:find_symbol:Alpha");

        let second = call_mcp_tool(
            &script,
            &root,
            &cancel,
            timeout,
            "impact",
            &json!({ "symbol": "Beta" }),
        )
        .await
        .unwrap();
        assert_eq!(second.text, "ok:impact:Beta");

        let spawns = fs::read_to_string(&counter_file).unwrap();
        assert_eq!(spawns.lines().count(), 1, "daemon should be spawned once");
        let methods = fs::read_to_string(&methods_file).unwrap();
        assert_eq!(
            methods.lines().collect::<Vec<_>>(),
            vec![
                "initialize",
                "notifications/initialized",
                "resources/read",
                "tools/call",
                "tools/call",
            ]
        );
    }

    #[tokio::test]
    async fn non_json_binary_detected_as_cli_only() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let script = write_script(
            dir.path(),
            "cli-only",
            "#!/usr/bin/env sh\necho 'not json rpc'\n",
        );

        let err = call_mcp_tool(
            &script,
            &root,
            &CancellationToken::new(),
            Duration::from_secs(5),
            "analyze",
            &json!({}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpCallError::CliOnly { .. }));
    }

    #[tokio::test]
    async fn slow_mcp_binary_returns_timeout_and_drops_daemon() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let script = write_script(dir.path(), "slow-mcp", "#!/usr/bin/env sh\nsleep 2\n");

        let err = call_mcp_tool(
            &script,
            &root,
            &CancellationToken::new(),
            Duration::from_millis(80),
            "analyze",
            &json!({}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpCallError::Timeout { .. }));
    }
}
