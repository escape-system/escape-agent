//! One language-server process and its JSON-RPC session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex as SyncMutex;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{broadcast, oneshot, watch};

use crate::config::LspServerConfig;
use crate::diagnostics::{CachedDiagnostics, DiagnosticsCache};
use crate::error::LspError;
use crate::language::language_id;
use crate::position::Position;
use crate::rpc::{self, FrameReader, RequestId, RpcError, RpcMessage};
use crate::uri::{path_to_uri, uri_to_path};
use crate::{LspDiagnosticsEvent, LspEvent};

/// `initialize` must complete within this window.
pub const INIT_TIMEOUT: Duration = Duration::from_secs(45);
/// Interactive requests time out after this long.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Files above this size are not synchronized.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Diagnostics events are coalesced by this delay.
pub const DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(150);
/// `edit`/`write` wait this long for diagnostics.
pub const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);

/// Everything needed to start one client.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Server definition (id, env, initialization).
    pub server: LspServerConfig,
    /// Canonical project root.
    pub root: PathBuf,
    /// Resolved argv (absolute executable first).
    pub argv: Vec<String>,
    /// Event fan-out shared by all clients.
    pub events: broadcast::Sender<LspEvent>,
    /// Initialize timeout.
    pub init_timeout: Duration,
    /// Per-request timeout.
    pub request_timeout: Duration,
    /// Diagnostics wait budget.
    pub diagnostics_wait: Duration,
}

struct Inner {
    server_id: String,
    server_name: String,
    root: PathBuf,
    root_uri: String,
    stdin: AsyncMutex<Option<ChildStdin>>,
    child: AsyncMutex<Option<Child>>,
    next_id: AtomicI64,
    pending: SyncMutex<HashMap<RequestId, oneshot::Sender<Result<Value, RpcError>>>>,
    diagnostics: SyncMutex<DiagnosticsCache>,
    open_files: SyncMutex<HashMap<PathBuf, i64>>,
    settings: Value,
    capabilities: SyncMutex<Value>,
    pull_supported: AtomicBool,
    events: broadcast::Sender<LspEvent>,
    in_flight: AtomicUsize,
    last_used: SyncMutex<Instant>,
    exited: AtomicBool,
    shutdown_started: AtomicBool,
    diag_seq: watch::Sender<u64>,
    debounce_pending: SyncMutex<HashSet<PathBuf>>,
    debouncing: AtomicBool,
    stderr: SyncMutex<String>,
    request_timeout: Duration,
    diagnostics_wait: Duration,
    pid: Option<u32>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClientInner")
            .field("server_id", &self.server_id)
            .field("root", &self.root)
            .field("exited", &self.exited.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// A live language-server session.
#[derive(Clone)]
pub struct LspClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl LspClient {
    /// Spawn the child and complete the `initialize` handshake.
    pub async fn spawn(request: SpawnRequest) -> Result<Self, LspError> {
        let SpawnRequest {
            server,
            root,
            argv,
            events,
            init_timeout,
            request_timeout,
            diagnostics_wait,
        } = request;
        let Some(program) = argv.first() else {
            return Err(LspError::Unavailable {
                server: server.id,
                reason: "no command configured".into(),
            });
        };

        let mut command = Command::new(program);
        command
            .args(argv.iter().skip(1))
            .current_dir(&root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Tokio kills the direct child on drop; unsafe Job Object code is
            // forbidden by the workspace lints.
            .kill_on_drop(true);
        if !server.env.is_empty() {
            command.envs(server.env.clone());
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|error| LspError::Spawn {
            server: server.id.clone(),
            message: error.to_string(),
            missing: error.kind() == std::io::ErrorKind::NotFound,
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let pid = child.id();

        let (diag_seq, _) = watch::channel(0_u64);
        let root_uri = path_to_uri(&root);
        let settings = server.initialization.clone().unwrap_or(Value::Null);
        let inner = Arc::new(Inner {
            server_id: server.id.clone(),
            server_name: server.name.clone(),
            root_uri,
            root: root.clone(),
            stdin: AsyncMutex::new(stdin),
            child: AsyncMutex::new(Some(child)),
            next_id: AtomicI64::new(1),
            pending: SyncMutex::new(HashMap::new()),
            diagnostics: SyncMutex::new(DiagnosticsCache::default()),
            open_files: SyncMutex::new(HashMap::new()),
            settings,
            capabilities: SyncMutex::new(Value::Null),
            pull_supported: AtomicBool::new(false),
            events,
            in_flight: AtomicUsize::new(0),
            last_used: SyncMutex::new(Instant::now()),
            exited: AtomicBool::new(false),
            shutdown_started: AtomicBool::new(false),
            diag_seq,
            debounce_pending: SyncMutex::new(HashSet::new()),
            debouncing: AtomicBool::new(false),
            stderr: SyncMutex::new(String::new()),
            request_timeout,
            diagnostics_wait,
            pid,
        });

        if let Some(stdout) = stdout {
            let reader = FrameReader::new(BufReader::new(stdout));
            tokio::spawn(read_loop(Arc::clone(&inner), reader));
        }
        if let Some(stderr) = stderr {
            tokio::spawn(stderr_loop(Arc::clone(&inner), stderr));
        }

        let client = Self { inner };
        match client.initialize(&server, init_timeout).await {
            Ok(()) => Ok(client),
            Err(error) => {
                client.force_kill().await;
                Err(error)
            }
        }
    }

    /// Server id from configuration.
    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    /// Human-readable server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.inner.server_name
    }

    /// Canonical root this client serves.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Capabilities returned by `initialize`.
    #[must_use]
    pub fn capabilities(&self) -> Value {
        self.inner.capabilities.lock().clone()
    }

    /// Whether the server advertised `textDocument/diagnostic`.
    #[must_use]
    pub fn supports_pull_diagnostics(&self) -> bool {
        self.inner.pull_supported.load(Ordering::Acquire)
    }

    /// Outstanding requests.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Relaxed)
    }

    /// When the client was last used by a tool.
    #[must_use]
    pub fn last_used(&self) -> Instant {
        *self.inner.last_used.lock()
    }

    /// The child process is gone.
    #[must_use]
    pub fn is_exited(&self) -> bool {
        self.inner.exited.load(Ordering::Acquire)
    }

    /// Last bytes written by the server to stderr, for error reporting.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.inner.stderr.lock().clone()
    }

    /// Child process id, when the platform exposes one.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.inner.pid
    }

    async fn initialize(
        &self,
        server: &LspServerConfig,
        init_timeout: Duration,
    ) -> Result<(), LspError> {
        let mut params = json!({
            "processId": std::process::id(),
            "clientInfo": {
                "name": "opencode",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "rootUri": self.inner.root_uri,
            "rootPath": self.inner.root.to_string_lossy(),
            "workspaceFolders": [self.workspace_folder()],
            "capabilities": client_capabilities(),
            "trace": "off",
        });
        if let Some(initialization) = &server.initialization {
            params["initializationOptions"] = initialization.clone();
        }

        let result = self
            .inner
            .request_timeout("initialize", params, init_timeout)
            .await
            .map_err(|error| match error {
                LspError::Timeout { .. } => LspError::Timeout {
                    method: "initialize".into(),
                    millis: u64::try_from(init_timeout.as_millis()).unwrap_or(u64::MAX),
                },
                other => other,
            })?;

        let mut capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        if capabilities.is_null() {
            capabilities = json!({});
        }
        let pull = capabilities
            .get("diagnosticProvider")
            .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)));
        self.inner.pull_supported.store(pull, Ordering::Release);
        *self.inner.capabilities.lock() = capabilities;

        self.notify("initialized", json!({})).await?;
        if let Some(initialization) = &server.initialization {
            self.notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": initialization }),
            )
            .await?;
        }
        Ok(())
    }

    fn workspace_folder(&self) -> Value {
        self.inner.workspace_folder()
    }

    /// Send a request and await its result.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let timeout = self.inner.request_timeout;
        self.inner.request_timeout(method, params, timeout).await
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.inner.touch();
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let message = without_null_params(message);
        self.inner.write(&message).await?;
        Ok(())
    }

    /// Synchronize a file with the server.
    ///
    /// The first touch sends `workspace/didChangeWatchedFiles` (created) and
    /// `textDocument/didOpen`; later touches send (changed) plus a full-text
    /// `didChange`. When `wait` is true, returns diagnostics after the
    /// documented debounce/wait budget.
    pub async fn touch_file(
        &self,
        path: &Path,
        wait: bool,
    ) -> Result<Option<CachedDiagnostics>, LspError> {
        let canonical = tokio::fs::canonicalize(path).await?;
        if !canonical.starts_with(&self.inner.root) {
            return Err(LspError::OutsideRoot(canonical));
        }
        let metadata = tokio::fs::metadata(&canonical).await?;
        if metadata.len() > MAX_FILE_BYTES {
            tracing::debug!(
                server = %self.inner.server_id,
                path = %canonical.display(),
                bytes = metadata.len(),
                "skipping LSP sync for a large file"
            );
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&tokio::fs::read(&canonical).await?).into_owned();
        let uri = path_to_uri(&canonical);
        let baseline = self.inner.diagnostics_sequence();

        self.sync_document(&canonical, &uri, &text).await?;
        self.pull_diagnostics(&canonical, &uri).await;

        if wait {
            Ok(self
                .inner
                .wait_for_diagnostics(&canonical, baseline, self.inner.diagnostics_wait)
                .await)
        } else {
            Ok(self.inner.diagnostics.lock().peek(&canonical))
        }
    }

    async fn sync_document(&self, canonical: &Path, uri: &str, text: &str) -> Result<(), LspError> {
        let version = {
            let mut open = self.inner.open_files.lock();
            if let Some(version) = open.get_mut(canonical) {
                *version = version.saturating_add(1);
                Some(*version)
            } else {
                open.insert(canonical.to_path_buf(), 0);
                None
            }
        };

        self.inner.touch();
        match version {
            None => {
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({ "changes": [{ "uri": uri, "type": 1 }] }),
                )
                .await?;
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(canonical),
                            "version": 0,
                            "text": text,
                        }
                    }),
                )
                .await?;
            }
            Some(version) => {
                self.notify(
                    "workspace/didChangeWatchedFiles",
                    json!({ "changes": [{ "uri": uri, "type": 2 }] }),
                )
                .await?;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn pull_diagnostics(&self, canonical: &Path, uri: &str) {
        if !self.supports_pull_diagnostics() {
            return;
        }
        let previous = self
            .inner
            .diagnostics
            .lock()
            .peek(canonical)
            .and_then(|entry| entry.result_id);
        let mut params = json!({ "textDocument": { "uri": uri } });
        if let Some(previous) = previous {
            params["previousResultId"] = Value::String(previous);
        }
        match self
            .inner
            .request_timeout(
                "textDocument/diagnostic",
                params,
                self.inner.request_timeout,
            )
            .await
        {
            Ok(result) => self.inner.apply_pull(canonical, &result),
            Err(error) => tracing::debug!(
                server = %self.inner.server_id,
                error = %error,
                "pull diagnostics failed; falling back to push"
            ),
        }
    }

    /// Wait for a diagnostics update for `path` (3 s budget by default).
    pub async fn wait_diagnostics(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Option<CachedDiagnostics> {
        self.inner.wait_for_diagnostics(path, 0, timeout).await
    }

    /// Cached diagnostics without waiting.
    pub fn cached_diagnostics(&self, path: &Path) -> Option<CachedDiagnostics> {
        self.inner.diagnostics.lock().get(path)
    }

    /// Wire position for a one-based user position, using the file's text.
    pub async fn wire_position(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Position, LspError> {
        let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
        let (position, _) = crate::position::user_position_to_offset(&text, line, character);
        Ok(position)
    }

    /// Send `textDocument/didClose` and drop cached diagnostics for `path`.
    pub async fn close_file(&self, path: &Path) {
        let canonical = tokio::fs::canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_path_buf());
        let was_open = self.inner.open_files.lock().remove(&canonical).is_some();
        if was_open {
            let uri = path_to_uri(&canonical);
            let _ = self
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri } }),
                )
                .await;
        }
        self.inner.diagnostics.lock().remove(&canonical);
    }

    /// `shutdown` → `exit` → 1 s → kill.
    pub async fn shutdown(&self) {
        if self.inner.shutdown_started.swap(true, Ordering::AcqRel) {
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while !self.is_exited() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            return;
        }
        let open_files: Vec<PathBuf> = self
            .inner
            .open_files
            .lock()
            .drain()
            .map(|(path, _)| path)
            .collect();
        for path in open_files {
            let uri = path_to_uri(&path);
            let _ = self
                .notify(
                    "textDocument/didClose",
                    json!({ "textDocument": { "uri": uri } }),
                )
                .await;
        }
        let _ = self
            .inner
            .request_timeout("shutdown", Value::Null, Duration::from_secs(2))
            .await;
        let _ = self.notify("exit", Value::Null).await;
        // EOF on stdin is the polite signal when the server ignores `exit`.
        self.inner.stdin.lock().await.take();
        self.inner.terminate_child().await;
        self.inner.fail_pending();
        self.inner.diagnostics.lock().clear();
        self.inner.exited.store(true, Ordering::Release);
    }

    /// Kill without the polite sequence (initialize failure, supervisor drop).
    pub async fn force_kill(&self) {
        self.inner.stdin.lock().await.take();
        self.inner.terminate_child().await;
        self.inner.fail_pending();
        self.inner.exited.store(true, Ordering::Release);
    }
}

impl Inner {
    fn workspace_folder(&self) -> Value {
        let name = self.root.file_name().map_or_else(
            || self.root.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        json!({ "uri": self.root_uri, "name": name })
    }

    fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    fn diagnostics_sequence(&self) -> u64 {
        *self.diag_seq.borrow()
    }

    fn bump_diagnostics(&self) {
        self.diag_seq.send_modify(|sequence| {
            *sequence = sequence.wrapping_add(1);
        });
    }

    async fn write(&self, message: &Value) -> Result<(), RpcError> {
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(RpcError::Closed);
        };
        rpc::write_message(stdin, message).await
    }

    async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let id = RequestId::Number(self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().insert(id.clone(), sender);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let message = without_null_params(message);
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        let written = self.write(&message).await;
        if let Err(error) = written {
            self.pending.lock().remove(&id);
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(error.into());
        }
        let outcome = tokio::time::timeout(timeout, receiver).await;
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.touch();
        match outcome {
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(LspError::Timeout {
                    method: method.to_owned(),
                    millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            }
            Ok(Err(_)) => {
                self.pending.lock().remove(&id);
                Err(RpcError::Closed.into())
            }
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Ok(Ok(result))) => Ok(result),
        }
    }

    fn fail_pending(&self) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, sender) in pending {
            let _ = sender.send(Err(RpcError::Closed));
        }
    }

    async fn terminate_child(&self) {
        let mut guard = self.child.lock().await;
        if let Some(child) = guard.as_mut()
            && tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .is_err()
        {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        }
        guard.take();
    }

    fn apply_pull(&self, path: &Path, result: &Value) {
        if let Some(kind) = result.get("kind").and_then(Value::as_str)
            && kind != "full"
        {
            return;
        }
        let items = match result.get("items").and_then(Value::as_array) {
            Some(items) => items.clone(),
            None => match result.as_array() {
                Some(items) => items.clone(),
                None => return,
            },
        };
        let result_id = result
            .get("resultId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.diagnostics.lock().put_pull(path, items, result_id);
        self.bump_diagnostics();
    }

    async fn wait_for_diagnostics(
        &self,
        path: &Path,
        baseline: u64,
        timeout: Duration,
    ) -> Option<CachedDiagnostics> {
        let mut receiver = self.diag_seq.subscribe();
        let deadline = Instant::now() + timeout;
        let debounce = DIAGNOSTICS_DEBOUNCE.min(timeout);
        tokio::time::sleep(debounce).await;
        let mut seen = baseline;
        loop {
            let sequence = self.diagnostics_sequence();
            if sequence > seen {
                seen = sequence;
                if let Some(entry) = self.diagnostics.lock().peek(path) {
                    return Some(entry);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.diagnostics.lock().peek(path);
            }
            if tokio::time::timeout(remaining, receiver.changed())
                .await
                .is_err()
            {
                return self.diagnostics.lock().peek(path);
            }
        }
    }

    fn append_stderr(&self, bytes: &[u8]) {
        const KEEP: usize = 8 * 1024;
        let mut buffer = self.stderr.lock();
        buffer.push_str(&String::from_utf8_lossy(bytes));
        if buffer.len() > KEEP {
            let mut cut = buffer.len() - KEEP;
            while cut < buffer.len() && !buffer.is_char_boundary(cut) {
                cut += 1;
            }
            buffer.drain(..cut);
        }
    }

    async fn dispatch(self: &Arc<Self>, message: RpcMessage) {
        match message {
            RpcMessage::Response { id, result, error } => {
                let Some(id) = id else {
                    return;
                };
                let Some(sender) = self.pending.lock().remove(&id) else {
                    return;
                };
                let outcome = match error {
                    Some(error) => Err(RpcError::Server {
                        code: error.code,
                        message: error.message,
                    }),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            RpcMessage::Request { id, method, params } => {
                self.handle_server_request(id, &method, &params).await;
            }
            RpcMessage::Notification { method, params } => {
                self.handle_notification(&method, &params);
            }
        }
    }

    async fn handle_server_request(&self, id: RequestId, method: &str, params: &Value) {
        let response = match method {
            "workspace/configuration" => {
                let items = params.get("items").and_then(Value::as_array);
                let values: Vec<Value> = items.map_or_else(Vec::new, |items| {
                    items
                        .iter()
                        .map(|item| {
                            let section = item
                                .get("section")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            configuration_value(&self.settings, section)
                        })
                        .collect()
                });
                json!({ "jsonrpc": "2.0", "id": id, "result": values })
            }
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null })
            }
            "workspace/workspaceFolders" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": [self.workspace_folder()] })
            }
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };
        if let Err(error) = self.write(&response).await {
            tracing::debug!(error = %error, "failed to answer server request");
        }
    }

    fn handle_notification(self: &Arc<Self>, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                    return;
                };
                let Some(path) = uri_to_path(uri) else {
                    return;
                };
                // Canonicalize so cache keys match `touch_file`'s canonical
                // paths (notably Windows verbatim prefixes).
                let path = std::fs::canonicalize(&path).unwrap_or(path);
                let Some(items) = params.get("diagnostics").and_then(Value::as_array) else {
                    return;
                };
                let version = params.get("version").and_then(Value::as_i64);
                let accepted = self
                    .diagnostics
                    .lock()
                    .put_push(&path, version, items.clone());
                if !accepted {
                    return;
                }
                self.bump_diagnostics();
                self.debounce_pending.lock().insert(path);
                schedule_debounce(Arc::clone(self));
            }
            "window/showMessage" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tracing::warn!(
                    server = %self.server_id,
                    message = %message,
                    "language server message"
                );
            }
            "window/logMessage" | "telemetry/event" | "$/progress" | "$/logTrace" => {}
            other => {
                tracing::trace!(server = %self.server_id, method = other, "ignored notification");
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Best-effort synchronous kill; `kill_on_drop` covers the async path.
        if let Ok(mut guard) = self.child.try_lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

fn without_null_params(mut message: Value) -> Value {
    if message.get("params") == Some(&Value::Null)
        && let Some(object) = message.as_object_mut()
    {
        object.remove("params");
    }
    message
}

fn configuration_value(settings: &Value, section: &str) -> Value {
    if section.is_empty() {
        return settings.clone();
    }
    let mut current = settings;
    for part in section.split('.') {
        match current.get(part) {
            Some(value) => current = value,
            None => return Value::Null,
        }
    }
    current.clone()
}

fn schedule_debounce(inner: Arc<Inner>) {
    if inner.debouncing.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(DIAGNOSTICS_DEBOUNCE).await;
        let paths: Vec<PathBuf> = inner.debounce_pending.lock().drain().collect();
        inner.debouncing.store(false, Ordering::Release);
        for path in paths {
            let _ = inner
                .events
                .send(LspEvent::Diagnostics(LspDiagnosticsEvent {
                    server_id: inner.server_id.clone(),
                    path: path.to_string_lossy().into_owned(),
                }));
        }
        if !inner.debounce_pending.lock().is_empty() {
            schedule_debounce(inner);
        }
    });
}

async fn read_loop(inner: Arc<Inner>, mut reader: FrameReader<BufReader<ChildStdout>>) {
    loop {
        match reader.read_message().await {
            Ok(Some(message)) => inner.dispatch(message).await,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(server = %inner.server_id, error = %error, "LSP reader stopping");
                break;
            }
        }
    }
    inner.fail_pending();
    let mut guard = inner.child.lock().await;
    if let Some(child) = guard.as_mut()
        && tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
    {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
    guard.take();
    drop(guard);
    inner.exited.store(true, Ordering::Release);
}

async fn stderr_loop(inner: Arc<Inner>, mut stderr: ChildStderr) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => inner.append_stderr(&buffer[..read]),
        }
    }
}

fn client_capabilities() -> Value {
    json!({
        "workspace": {
            "applyEdit": false,
            "workspaceEdit": { "documentChanges": false },
            "configuration": true,
            "workspaceFolders": true,
            "didChangeConfiguration": { "dynamicRegistration": true },
            "didChangeWatchedFiles": { "dynamicRegistration": true },
            "symbol": { "dynamicRegistration": false },
            "executeCommand": { "dynamicRegistration": false }
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": false
            },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "codeDescriptionSupport": true,
                "dataSupport": true,
                "tagSupport": { "valueSet": [1, 2] }
            },
            "diagnostic": { "dynamicRegistration": false, "relatedDocumentSupport": false },
            "hover": {
                "dynamicRegistration": false,
                "contentFormat": ["markdown", "plaintext"]
            },
            "definition": { "dynamicRegistration": false, "linkSupport": true },
            "declaration": { "dynamicRegistration": false, "linkSupport": true },
            "references": { "dynamicRegistration": false },
            "implementation": { "dynamicRegistration": false, "linkSupport": true },
            "documentSymbol": {
                "dynamicRegistration": false,
                "hierarchicalDocumentSymbolSupport": true,
                "symbolKind": { "valueSet": [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26] }
            },
            "callHierarchy": { "dynamicRegistration": false }
        },
        "window": { "workDoneProgress": true },
        "general": {
            "positionEncodings": ["utf-16"],
            "staleRequestSupport": { "cancel": true, "retryOnContentModified": [] }
        }
    })
}
