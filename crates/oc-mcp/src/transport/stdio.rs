//! stdio MCP transport: newline-delimited JSON over a child process.
//!
//! The child runs with a private process group on Unix so shutdown can signal
//! the whole tree; on Windows `taskkill /T` does the same. stderr is drained
//! continuously and the last 1 KiB is kept for exit diagnostics.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use crate::error::{McpError, McpResult, RpcError};
use crate::protocol::{Message, NewlineDecoder, Notification, Request, RequestId, Response, codes};
use crate::transport::{Transport, TransportEventHandler, TransportFactory};

/// Grace period for a child to exit after stdin closes.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// SIGTERM to SIGKILL escalation delay.
const FORCE_KILL_AFTER: Duration = Duration::from_secs(2);
/// How long to wait for the exit code after stdout closes.
const EXIT_WAIT: Duration = Duration::from_secs(1);
/// Outgoing frame queue depth (upstream: 64).
const OUTGOING_CAPACITY: usize = 64;
/// stderr bytes retained for diagnostics.
const STDERR_TAIL: usize = 1_000;

/// Options for spawning a stdio server.
#[derive(Debug, Clone)]
pub struct StdioOptions {
    /// Server name, used only for logs and diagnostics.
    pub server: String,
    /// `command[0]` is the executable; the rest are arguments.
    pub command: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Explicit environment variables; the host environment is inherited.
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug)]
enum Outgoing {
    Line(Vec<u8>),
    Close,
}

#[derive(Debug)]
struct Shared {
    server: String,
    tx: mpsc::Sender<Outgoing>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<McpResult<Value>>>>,
    next_id: AtomicI64,
    handler: RwLock<Option<Arc<dyn TransportEventHandler>>>,
    stderr: Mutex<Vec<u8>>,
    process: Mutex<Option<Child>>,
    closed: AtomicBool,
    close_notified: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn handler(&self) -> Option<Arc<dyn TransportEventHandler>> {
        self.handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn fail_pending(&self, make: impl Fn() -> McpError) {
        let pending = std::mem::take(&mut *lock(&self.pending));
        for (_, sender) in pending {
            let _ = sender.send(Err(make()));
        }
    }

    fn stderr_tail(&self) -> String {
        let bytes = lock(&self.stderr).clone();
        String::from_utf8_lossy(&bytes).trim().to_owned()
    }

    fn push_stderr(&self, chunk: &[u8]) {
        let mut tail = lock(&self.stderr);
        tail.extend_from_slice(chunk);
        let excess = tail.len().saturating_sub(STDERR_TAIL);
        if excess > 0 {
            tail.drain(..excess);
        }
    }

    async fn send_message(&self, message: &Message) -> McpResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed);
        }
        let line = message.to_line()?;
        self.tx
            .send(Outgoing::Line(line))
            .await
            .map_err(|_| McpError::Closed)
    }
}

/// A stdio transport.
#[derive(Debug)]
pub struct StdioTransport {
    shared: Arc<Shared>,
}

impl StdioTransport {
    /// Spawns the child process and its reader/writer/stderr tasks.
    pub fn spawn(options: StdioOptions) -> McpResult<Arc<Self>> {
        let Some(program) = options.command.first() else {
            return Err(McpError::Config("empty command".to_owned()));
        };
        let mut command = Command::new(program);
        command
            .args(&options.command[1..])
            .current_dir(&options.cwd)
            .envs(&options.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if program == "opencode" {
            command.env("BUN_BE_BUN", "1");
        }
        #[cfg(unix)]
        {
            // Own process group: the whole tree can be signalled on shutdown.
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| {
            McpError::Transport(format!(
                "failed to spawn {}: {error}",
                options.command.join(" ")
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child stdin unavailable".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child stdout unavailable".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpError::Transport("child stderr unavailable".to_owned()))?;

        let (tx, rx) = mpsc::channel(OUTGOING_CAPACITY);
        let shared = Arc::new(Shared {
            server: options.server,
            tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            handler: RwLock::new(None),
            stderr: Mutex::new(Vec::new()),
            process: Mutex::new(Some(child)),
            closed: AtomicBool::new(false),
            close_notified: AtomicBool::new(false),
        });

        tokio::spawn(writer_loop(stdin, rx));
        tokio::spawn(stderr_loop(shared.clone(), stderr));
        tokio::spawn(reader_loop(shared.clone(), stdout));
        Ok(Arc::new(Self { shared }))
    }

    fn request_id(&self) -> RequestId {
        RequestId::Number(self.shared.next_id.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

async fn writer_loop(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<Outgoing>) {
    while let Some(item) = rx.recv().await {
        match item {
            Outgoing::Line(line) => {
                if stdin.write_all(&line).await.is_err() {
                    break;
                }
                if stdin.write_all(b"\n").await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            Outgoing::Close => break,
        }
    }
    // Dropping stdin signals EOF to the child.
    drop(stdin);
}

async fn stderr_loop(shared: Arc<Shared>, mut stderr: tokio::process::ChildStderr) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                shared.push_stderr(&buffer[..read]);
                let text = String::from_utf8_lossy(&buffer[..read]);
                if !text.trim().is_empty() {
                    tracing::debug!(server = %shared.server, output = %text.trim_end(), "mcp server stderr");
                }
            }
        }
    }
}

async fn reader_loop(shared: Arc<Shared>, mut stdout: tokio::process::ChildStdout) {
    let mut decoder = NewlineDecoder::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(server = %shared.server, %error, "mcp stdout read failed");
                break;
            }
        };
        let (frames, capped) = decoder.feed(&buffer[..read]);
        if let Some(error) = capped
            && let Some(handler) = shared.handler()
        {
            handler.on_error(error.to_string());
        }
        for frame in frames {
            match Message::parse(&String::from_utf8_lossy(&frame)) {
                Ok(message) => dispatch(&shared, message),
                Err(error) => {
                    if let Some(handler) = shared.handler() {
                        handler.on_error(format!("invalid MCP stdio frame: {error}"));
                    }
                }
            }
        }
    }

    if shared.closed.swap(true, Ordering::SeqCst) {
        return;
    }
    let reason = exit_reason(&shared).await;
    shared.fail_pending(|| McpError::Exited(reason.clone()));
    if let Some(handler) = shared.handler()
        && !shared.close_notified.swap(true, Ordering::SeqCst)
    {
        handler.on_close(reason).await;
    }
}

fn dispatch(shared: &Arc<Shared>, message: Message) {
    match message {
        Message::Response(response) => {
            let sender = lock(&shared.pending).remove(&response.id);
            match sender {
                Some(sender) => {
                    let result = match (response.result, response.error) {
                        (_, Some(error)) => Err(McpError::Rpc(error)),
                        (Some(result), None) => Ok(result),
                        (None, None) => Err(McpError::Protocol(
                            "response has neither result nor error".to_owned(),
                        )),
                    };
                    let _ = sender.send(result);
                }
                None => {
                    tracing::debug!(server = %shared.server, id = %response.id, "unmatched MCP response");
                }
            }
        }
        Message::Request(request) => {
            let shared = shared.clone();
            tokio::spawn(async move {
                let result = match shared.handler() {
                    Some(handler) => handler.on_request(&request.method, request.params).await,
                    None => Err(RpcError {
                        code: codes::METHOD_NOT_FOUND,
                        message: format!("unsupported method: {}", request.method),
                        data: None,
                    }),
                };
                let response = match result {
                    Ok(result) => Response::success(request.id, result),
                    Err(error) => Response::failure(request.id, error),
                };
                let _ = shared.send_message(&Message::Response(response)).await;
            });
        }
        Message::Notification(notification) => {
            if let Some(handler) = shared.handler() {
                let method = notification.method;
                let params = notification.params;
                tokio::spawn(async move {
                    handler.on_notification(&method, params).await;
                });
            }
        }
    }
}

async fn exit_reason(shared: &Shared) -> String {
    let started = tokio::time::Instant::now();
    let mut status = None;
    while started.elapsed() < EXIT_WAIT {
        let exited = {
            let mut guard = lock(&shared.process);
            match guard.as_mut() {
                Some(child) => child.try_wait().ok().flatten(),
                None => None,
            }
        };
        if let Some(exited) = exited {
            status = Some(exited);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let tail = shared.stderr_tail();
    let base = match status {
        Some(status) => match status.code() {
            Some(code) => format!("MCP server process exited with code {code}"),
            None => "MCP server process exited".to_owned(),
        },
        None => "MCP server process exited".to_owned(),
    };
    if tail.is_empty() {
        base
    } else {
        format!("{base}: {tail}")
    }
}

fn kill_tree(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    if let Some(pid) = pid {
        // Negative pid signals the process group created at spawn.
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.start_kill();
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if let Some(mut child) = lock(&self.shared.process).take() {
            kill_tree(&mut child);
        }
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> McpResult<Value> {
        let id = self.request_id();
        let (tx, rx) = oneshot::channel();
        lock(&self.shared.pending).insert(id.clone(), tx);
        let request = Message::Request(Request::new(id.clone(), method, params));
        if let Err(error) = self.shared.send_message(&request).await {
            lock(&self.shared.pending).remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::Closed),
            Err(_) => {
                lock(&self.shared.pending).remove(&id);
                let cancelled = Notification::new(
                    "notifications/cancelled",
                    Some(serde_json::json!({
                        "requestId": id,
                        "reason": format!("request timed out after {timeout:?}"),
                    })),
                );
                let _ = self
                    .shared
                    .send_message(&Message::Notification(cancelled))
                    .await;
                Err(McpError::Timeout(timeout))
            }
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) -> McpResult<()> {
        self.shared
            .send_message(&Message::Notification(Notification::new(method, params)))
            .await
    }

    fn set_event_handler(&self, handler: Arc<dyn TransportEventHandler>) {
        *self
            .shared
            .handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    fn pid(&self) -> Option<u32> {
        lock(&self.shared.process).as_ref().and_then(Child::id)
    }

    async fn shutdown(&self) {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.shared.tx.send(Outgoing::Close).await;
        let child = lock(&self.shared.process).take();
        if let Some(mut child) = child
            && tokio::time::timeout(CLOSE_GRACE, child.wait())
                .await
                .is_err()
        {
            kill_tree(&mut child);
            let _ = tokio::time::timeout(FORCE_KILL_AFTER, child.wait()).await;
        }
        self.shared.fail_pending(|| McpError::Closed);
    }
}

/// Builds fresh stdio transports for reconnect.
#[derive(Debug)]
pub struct StdioFactory {
    options: StdioOptions,
}

impl StdioFactory {
    /// Creates a factory from spawn options.
    #[must_use]
    pub fn new(options: StdioOptions) -> Self {
        Self { options }
    }
}

#[async_trait]
impl TransportFactory for StdioFactory {
    async fn create(&self) -> McpResult<Arc<dyn Transport>> {
        let transport = StdioTransport::spawn(self.options.clone())?;
        Ok(transport)
    }
}
