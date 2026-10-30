//! `/api/shell` and the persistent-PTY aliases.
//!
//! [`ShellManager`] runs one non-interactive command per shell: combined
//! stdout/stderr are appended to a bounded in-process ring (1 MiB) while the
//! same bytes are captured in a temp file exposed as `Shell.Info.file`. Output
//! is paged by absolute byte cursor; evicted cursors are clamped to the oldest
//! retained byte and reported with `truncated: true`.
//!
//! The manager is process-global ([`manager`]); exited shells stay observable
//! until [`EXITED_LIMIT`] newer ones have exited or a client deletes them.
//!
//! # HTTP
//!
//! - `GET /api/shell` → running shells in a location envelope.
//! - `POST /api/shell` → `{command, cwd?, timeout?, metadata?}`, publishes
//!   `shell.created`.
//! - `GET /api/shell/:id` → one `Shell.Info` (running or exited).
//! - `DELETE /api/shell/:id` → kill the process tree when running, remove the
//!   temp file, publish `shell.exited` (when it was running) then
//!   `shell.deleted`, answer 204.
//! - `GET /api/shell/:id/output?cursor&limit` → `{output, cursor, size,
//!   truncated}`.
//! - `POST /api/session/:sid/shell` → start in the session directory with
//!   `OPENCODE_SESSION_ID` set, answer 204.
//! - `GET|POST /api/experimental/session/:sid/terminal`, `GET .../read` and the
//!   `/api/experimental/persistent-pty*` aliases expose [`oc_pty::PtyManager`]
//!   terminals that were created through them, bound to a session id.
//!
//! The persistent-PTY surface is an alias over `oc-pty`, which retains raw
//! bytes rather than a terminal cell model. `read`/`snapshot` therefore report
//! the retained text and an end-of-output cursor; the checkpoint is the raw
//! retained byte stream. No screen content is fabricated.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use oc_pty::{CreatePty, PtyError, PtyInfo, PtyManager};
use oc_types::{ApiError, DataEnvelope, EventEnvelope, EventLocation, LocationEnvelope};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Notify, broadcast};
use tokio::time::timeout;

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error, require_store, schema_error, session_not_found};
use crate::error::ErrorResponse;

/// Retained bytes of combined stdout/stderr per shell.
const SHELL_BUFFER: usize = 1024 * 1024;

/// Exited shells kept observable before the oldest is evicted.
const EXITED_LIMIT: usize = 16;

/// Upstream's default page size for `output`.
const OUTPUT_LIMIT: usize = 64 * 1024;

/// How long `DELETE` waits for a killed child before removing it anyway.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// How long the exit watcher waits for the output reader to drain the file.
const DRAIN_WAIT: Duration = Duration::from_secs(5);

const SHELL_DIR: &str = "opencode-shell";
const TICKET_HEADER: &str = "x-opencode-ticket";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Creates a capture file with an unpredictable suffix using `create_new`, so
/// a predictable name can never be truncated or hijacked through a symlink.
fn create_capture(directory: &Path, id: &str) -> Result<(PathBuf, std::fs::File), ApiError> {
    for _ in 0..8 {
        let candidate = directory.join(format!("{id}-{:016x}.out", rand::random::<u64>()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ApiError::internal(format!(
                    "unable to capture shell output: {error}"
                )));
            }
        }
    }
    Err(ApiError::internal(
        "unable to allocate a unique shell capture file",
    ))
}

// ---------------------------------------------------------------------------
// Shell manager
// ---------------------------------------------------------------------------

static MANAGER: LazyLock<Arc<ShellManager>> = LazyLock::new(|| Arc::new(ShellManager::new()));

/// The process-global shell manager.
pub fn manager() -> &'static Arc<ShellManager> {
    LazyLock::force(&MANAGER)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Exited,
    Timeout,
    Killed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Timeout => "timeout",
            Self::Killed => "killed",
        }
    }
}

/// Outcome claimed by the exit watcher, the timeout and `cancel`; the first
/// writer wins so a timeout that races a kill reports one terminal status.
const OUTCOME_PENDING: u8 = 0;
const OUTCOME_EXITED: u8 = 1;
const OUTCOME_TIMEOUT: u8 = 2;
const OUTCOME_KILLED: u8 = 3;

struct InfoState {
    status: Status,
    command: String,
    cwd: String,
    shell: String,
    metadata: Value,
    exit: Option<i32>,
    signal: Option<String>,
    started: i64,
    completed: Option<i64>,
}

/// Bounded, absolute-cursor byte window over combined output.
struct Output {
    data: Vec<u8>,
    head: usize,
    capacity: usize,
    start: u64,
    size: u64,
}

impl Output {
    fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity.min(128 * 1024)),
            head: 0,
            capacity: capacity.max(1),
            start: 0,
            size: 0,
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        self.size = self
            .size
            .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        self.data.extend_from_slice(chunk);
        let live = self.data.len() - self.head;
        if live <= self.capacity {
            return;
        }
        let mut cut = self.head + (live - self.capacity);
        // Do not start the retained window on a UTF-8 continuation byte.
        while cut < self.data.len() && self.data[cut] & 0xC0 == 0x80 {
            cut += 1;
        }
        self.start = self
            .start
            .saturating_add(u64::try_from(cut - self.head).unwrap_or(u64::MAX));
        self.head = cut;
        let compact_at = (self.capacity / 8).max(1);
        if self.head >= compact_at {
            self.data.drain(..self.head);
            self.head = 0;
        }
    }

    /// (text, cursor after this page, whether the requested cursor was
    /// evicted). A page never crosses the retained window's end.
    fn read(&self, cursor: u64, limit: usize) -> (String, u64, bool) {
        let mut truncated = false;
        let mut start = cursor;
        if start < self.start {
            start = self.start;
            truncated = true;
        }
        if start > self.size {
            start = self.size;
        }
        let end = self
            .size
            .min(start.saturating_add(u64::try_from(limit).unwrap_or(u64::MAX)));
        let from = self.head + usize::try_from(start - self.start).unwrap_or(0);
        let to = self.head + usize::try_from(end - self.start).unwrap_or(0);
        let to = to.min(self.data.len());
        let from = from.min(to);
        (
            String::from_utf8_lossy(&self.data[from..to]).into_owned(),
            end,
            truncated,
        )
    }
}

struct Shell {
    id: String,
    pid: u32,
    file: PathBuf,
    info: Mutex<InfoState>,
    output: Mutex<Output>,
    outcome: AtomicU8,
    process_done: AtomicBool,
    output_done: AtomicBool,
    output_notify: Notify,
    finished: AtomicBool,
    notify: Notify,
    bus: Option<broadcast::Sender<Arc<EventEnvelope>>>,
    location: Option<EventLocation>,
    location_directory: Option<String>,
}

impl Shell {
    fn status(&self) -> Status {
        lock(&self.info).status
    }

    fn wire_info(&self) -> Value {
        let info = lock(&self.info);
        let mut value = Map::new();
        value.insert("id".to_owned(), json!(self.id));
        value.insert("status".to_owned(), json!(info.status.as_str()));
        value.insert("command".to_owned(), json!(info.command));
        value.insert("cwd".to_owned(), json!(info.cwd));
        value.insert("shell".to_owned(), json!(info.shell));
        value.insert(
            "file".to_owned(),
            json!(self.file.to_string_lossy().into_owned()),
        );
        if self.pid != 0 {
            value.insert("pid".to_owned(), json!(self.pid));
        }
        if let Some(exit) = info.exit {
            value.insert("exit".to_owned(), json!(exit));
        }
        if let Some(signal) = &info.signal {
            value.insert("signal".to_owned(), json!(signal));
        }
        value.insert("metadata".to_owned(), info.metadata.clone());
        let mut time = Map::new();
        time.insert("started".to_owned(), json!(info.started));
        if let Some(completed) = info.completed {
            time.insert("completed".to_owned(), json!(completed));
        }
        value.insert("time".to_owned(), Value::Object(time));
        Value::Object(value)
    }

    fn publish(&self, kind: &str, data: Value) {
        let Some(bus) = &self.bus else {
            return;
        };
        let mut event = EventEnvelope::now(kind, data);
        event.location.clone_from(&self.location);
        let _ = bus.send(Arc::new(event));
    }

    /// Claims the terminal outcome if nobody claimed it yet.
    fn claim(&self, outcome: u8) -> bool {
        self.outcome
            .compare_exchange(OUTCOME_PENDING, outcome, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Requests a tree kill; the exit watcher reports `killed` when the claim
    /// was still pending.
    fn request_kill(&self) {
        if self.claim(OUTCOME_KILLED) {
            let pid = self.pid;
            tokio::spawn(async move { kill_tree(pid).await });
        }
    }
}

#[derive(Default)]
struct Registry {
    shells: HashMap<String, Arc<Shell>>,
    exited: VecDeque<String>,
}

/// In-process non-interactive shell command manager.
pub struct ShellManager {
    registry: Arc<Mutex<Registry>>,
    next_id: AtomicU64,
}

/// Request to start one shell command.
#[derive(Debug)]
pub struct CreateShell {
    pub command: String,
    pub cwd: PathBuf,
    pub shell: Option<String>,
    pub env: BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub metadata: Value,
    pub bus: Option<broadcast::Sender<Arc<EventEnvelope>>>,
    pub location: Option<EventLocation>,
    pub location_directory: Option<String>,
}

impl std::fmt::Debug for ShellManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let registry = lock(&self.registry);
        f.debug_struct("ShellManager")
            .field("shells", &registry.shells.len())
            .field("exited", &registry.exited.len())
            .finish_non_exhaustive()
    }
}

impl Default for ShellManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry::default())),
            next_id: AtomicU64::new(0),
        }
    }

    fn generate_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("sh_{:012x}{sequence:04x}", now_ms().max(0))
    }

    fn entry(&self, id: &str) -> Result<Arc<Shell>, ApiError> {
        lock(&self.registry)
            .shells
            .get(id)
            .cloned()
            .ok_or_else(|| shell_not_found(id))
    }

    /// Starts a command and returns its wire `Shell.Info`.
    #[allow(clippy::too_many_lines)] // spawn, capture, reader and watcher are one flow
    pub async fn create(&self, req: CreateShell) -> Result<Value, ApiError> {
        if req.command.trim().is_empty() {
            return Err(ApiError::new(
                400,
                "InvalidRequestError",
                "command must not be empty",
            ));
        }
        let id = self.generate_id();
        let (shell, kind) = resolve_shell(req.shell.as_deref());
        let args = kind.args(&req.command);
        let cwd = if req.cwd.as_os_str().is_empty() {
            std::env::current_dir().map_err(|error| {
                ApiError::internal(format!("unable to resolve the working directory: {error}"))
            })?
        } else {
            req.cwd.clone()
        };

        let directory = std::env::temp_dir().join(SHELL_DIR);
        std::fs::create_dir_all(&directory).map_err(|error| {
            ApiError::internal(format!(
                "unable to create the shell output directory: {error}"
            ))
        })?;
        let (file, capture) = create_capture(&directory, &id)?;
        let stdout = capture.try_clone().map_err(|error| {
            ApiError::internal(format!("unable to capture shell output: {error}"))
        })?;
        let stderr = capture.try_clone().map_err(|error| {
            ApiError::internal(format!("unable to capture shell output: {error}"))
        })?;
        drop(capture);

        let mut command = Command::new(&shell);
        command.args(&args);
        command
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::from(stderr))
            .env("TERM", "xterm-256color")
            .env("OPENCODE_TERMINAL", "1")
            .env("OPENCODE", "1")
            .env("AGENT", "1")
            .kill_on_drop(true);
        for (key, value) in &req.env {
            command.env(key, value);
        }
        #[cfg(unix)]
        command.process_group(0);

        let child = command.spawn().map_err(|error| {
            let _ = std::fs::remove_file(&file);
            ApiError::new(
                400,
                "InvalidRequestError",
                format!("unable to start shell command: {error}"),
            )
        })?;
        let pid = child.id().unwrap_or(0);

        let shell_entry = Arc::new(Shell {
            id: id.clone(),
            pid,
            file: file.clone(),
            info: Mutex::new(InfoState {
                status: Status::Running,
                command: req.command,
                cwd: cwd.to_string_lossy().into_owned(),
                shell,
                metadata: req.metadata,
                exit: None,
                signal: None,
                started: now_ms(),
                completed: None,
            }),
            output: Mutex::new(Output::new(SHELL_BUFFER)),
            outcome: AtomicU8::new(OUTCOME_PENDING),
            process_done: AtomicBool::new(false),
            output_done: AtomicBool::new(false),
            output_notify: Notify::new(),
            finished: AtomicBool::new(false),
            notify: Notify::new(),
            bus: req.bus,
            location: req.location,
            location_directory: req.location_directory,
        });

        lock(&self.registry)
            .shells
            .insert(id.clone(), Arc::clone(&shell_entry));

        let reader = match tokio::fs::File::open(&file).await {
            Ok(reader) => reader,
            Err(error) => {
                shell_entry.request_kill();
                let _ = self.remove_entry(&id);
                let _ = tokio::fs::remove_file(&file).await;
                return Err(ApiError::internal(format!(
                    "unable to read shell output: {error}"
                )));
            }
        };
        tokio::spawn(read_output(Arc::clone(&shell_entry), reader));

        let registry = Arc::downgrade(&self.registry);
        let timeout = req.timeout.filter(|duration| !duration.is_zero());
        tokio::spawn(wait_for_exit(shell_entry.clone(), child, registry, timeout));

        let info = shell_entry.wire_info();
        shell_entry.publish("shell.created", json!({ "info": info.clone() }));
        Ok(info)
    }

    /// Running shells only, oldest first.
    #[must_use]
    #[allow(clippy::needless_collect)] // the collect releases the registry lock
    pub fn list(&self, directory: Option<&str>) -> Vec<Value> {
        let shells: Vec<Arc<Shell>> = lock(&self.registry)
            .shells
            .values()
            .filter(|shell| shell.status() == Status::Running)
            .filter(|shell| match directory {
                Some(directory) => shell.location_directory.as_deref() == Some(directory),
                None => true,
            })
            .cloned()
            .collect();
        let mut values: Vec<(i64, Value)> = shells
            .iter()
            .map(|shell| {
                let started = lock(&shell.info).started;
                (started, shell.wire_info())
            })
            .collect();
        values.sort_by_key(|entry| entry.0);
        values.into_iter().map(|(_, value)| value).collect()
    }

    /// One shell (running or retained) as a wire `Shell.Info`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Value> {
        lock(&self.registry)
            .shells
            .get(id)
            .map(|shell| shell.wire_info())
    }

    /// Pages captured output. `cursor` defaults to `0` and `limit` to 64 KiB.
    pub fn output(&self, id: &str, cursor: u64, limit: usize) -> Result<Value, ApiError> {
        let shell = self.entry(id)?;
        let output = lock(&shell.output);
        let (text, cursor, truncated) = output.read(cursor, limit);
        Ok(json!({
            "output": text,
            "cursor": cursor,
            "size": output.size,
            "truncated": truncated,
        }))
    }

    /// Terminates a running shell tree but keeps it observable as `killed`.
    pub fn cancel(&self, id: &str) -> Result<(), ApiError> {
        self.entry(id)?.request_kill();
        Ok(())
    }

    /// Ends (when needed) and forgets one shell, removing its capture file.
    pub async fn remove(&self, id: &str) -> Result<(), ApiError> {
        let shell = self.entry(id)?;
        if shell.status() == Status::Running {
            shell.request_kill();
        }
        wait_finished(&shell, EXIT_WAIT).await;
        self.remove_entry(id)?;
        let _ = tokio::fs::remove_file(&shell.file).await;
        shell.publish("shell.deleted", json!({ "id": id }));
        Ok(())
    }

    fn remove_entry(&self, id: &str) -> Result<Arc<Shell>, ApiError> {
        let mut registry = lock(&self.registry);
        let shell = registry
            .shells
            .remove(id)
            .ok_or_else(|| shell_not_found(id))?;
        registry.exited.retain(|existing| existing != id);
        Ok(shell)
    }

    /// Kills every running shell and removes its capture file.
    pub fn shutdown(&self) {
        let shells: Vec<Arc<Shell>> = {
            let mut registry = lock(&self.registry);
            registry.exited.clear();
            registry.shells.drain().map(|(_, shell)| shell).collect()
        };
        for shell in &shells {
            if shell.status() == Status::Running {
                kill_tree_blocking(shell.pid);
            }
            let _ = std::fs::remove_file(&shell.file);
        }
    }
}

impl Drop for ShellManager {
    fn drop(&mut self) {
        let shells: Vec<Arc<Shell>> = lock(&self.registry).shells.values().cloned().collect();
        for shell in shells {
            if shell.status() == Status::Running {
                kill_tree_blocking(shell.pid);
            }
        }
    }
}

async fn read_output(shell: Arc<Shell>, mut reader: tokio::fs::File) {
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

async fn wait_for_exit(
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

async fn wait_finished(shell: &Shell, limit: Duration) {
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
async fn kill_tree(pid: u32) {
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

fn kill_tree_blocking(pid: u32) {
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

// ---------------------------------------------------------------------------
// Shell selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Cmd,
    PowerShell,
    Posix,
}

impl ShellKind {
    fn args(self, command: &str) -> Vec<String> {
        match self {
            Self::Cmd => vec!["/c".to_owned(), command.to_owned()],
            Self::PowerShell => vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                command.to_owned(),
            ],
            Self::Posix => vec!["-c".to_owned(), command.to_owned()],
        }
    }
}

fn kind_of(program: &Path) -> ShellKind {
    let name = program
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "cmd" {
        ShellKind::Cmd
    } else if name == "pwsh" || name == "powershell" {
        ShellKind::PowerShell
    } else {
        ShellKind::Posix
    }
}

/// Resolves the configured shell, falling back to the platform default when
/// it is not installed (the config default is `pwsh` even on hosts without it).
fn resolve_shell(requested: Option<&str>) -> (String, ShellKind) {
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty())
        && let Some(path) = find_program(requested)
    {
        let kind = kind_of(&path);
        return (path.to_string_lossy().into_owned(), kind);
    }
    let path = platform_shell();
    let kind = kind_of(&path);
    (path.to_string_lossy().into_owned(), kind)
}

#[cfg(windows)]
fn platform_shell() -> PathBuf {
    for candidate in ["pwsh", "powershell"] {
        if let Some(path) = find_program(candidate) {
            return path;
        }
    }
    if let Some(comspec) = std::env::var_os("COMSPEC") {
        return PathBuf::from(comspec);
    }
    PathBuf::from("cmd.exe")
}

#[cfg(unix)]
fn platform_shell() -> PathBuf {
    if let Some(shell) = std::env::var_os("SHELL") {
        let path = PathBuf::from(shell);
        if path.is_file() {
            return path;
        }
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh", "/usr/bin/sh"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("sh")
}

fn find_program(program: &str) -> Option<PathBuf> {
    let direct = Path::new(program);
    if direct.is_absolute() || direct.components().count() > 1 {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if direct.extension().is_none() {
                for extension in ["exe", "cmd", "bat", "com"] {
                    let candidate = directory.join(format!("{program}.{extension}"));
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
// `/api/shell` handlers
// ---------------------------------------------------------------------------

/// Shell routes; nested by the router owner.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/shell", get(list).post(create))
        .route("/api/shell/{id}", get(get_one).delete(remove))
        .route("/api/shell/{id}/output", get(output))
        .route("/api/session/{sid}/shell", post(session_shell))
        .route(
            "/api/experimental/session/{sid}/terminal",
            get(terminal_list).post(terminal_create),
        )
        .route(
            "/api/experimental/session/{sid}/terminal/read",
            get(terminal_read),
        )
        .route(
            "/api/experimental/persistent-pty",
            get(persistent_list).post(persistent_create),
        )
        .route(
            "/api/experimental/persistent-pty/{id}",
            get(persistent_get)
                .put(persistent_update)
                .delete(persistent_remove),
        )
        .route(
            "/api/experimental/persistent-pty/{id}/snapshot",
            get(persistent_snapshot),
        )
        .route(
            "/api/experimental/persistent-pty/{id}/connect-token",
            post(persistent_connect_token),
        )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CreateBody {
    command: String,
    cwd: Option<String>,
    /// Milliseconds; zero or absent means no timeout.
    timeout: Option<u64>,
    metadata: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OutputQuery {
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

/// `GET /api/shell`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let shells = manager().list(Some(resolved.directory.as_str()));
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        json!(shells),
    )))
}

/// `POST /api/shell`
pub(crate) async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let metadata = metadata_object(payload.metadata)?;
    let cwd = payload
        .cwd
        .filter(|value| !value.is_empty())
        .map_or_else(|| resolved.path(), PathBuf::from);
    let info = manager()
        .create(CreateShell {
            command: payload.command,
            cwd,
            shell: configured_shell(&state),
            env: BTreeMap::new(),
            timeout: payload.timeout.map(Duration::from_millis),
            metadata,
            bus: Some(state.bus().clone()),
            location: Some(resolved.event()),
            location_directory: Some(resolved.directory.clone()),
        })
        .await
        .map_err(ErrorResponse::from)?;
    Ok(Json(LocationEnvelope::new(resolved.reference(), info)))
}

/// `GET /api/shell/:id`
pub async fn get_one(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let info = manager()
        .get(&id)
        .ok_or_else(|| ErrorResponse::from(shell_not_found(&id)))?;
    Ok(Json(LocationEnvelope::new(resolved.reference(), info)))
}

/// `GET /api/shell/:id/output`
pub(crate) async fn output(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<OutputQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let cursor = parse_number(params.cursor.as_deref(), "cursor")?;
    let limit = match params.limit.as_deref() {
        Some(raw) => usize::try_from(parse_number(Some(raw), "limit")?).map_err(|_| {
            ErrorResponse::from(ApiError::new(
                400,
                "InvalidRequestError",
                "limit is out of range",
            ))
        })?,
        None => OUTPUT_LIMIT,
    };
    let page = manager()
        .output(&id, cursor, limit)
        .map_err(ErrorResponse::from)?;
    Ok(Json(LocationEnvelope::new(resolved.reference(), page)))
}

/// `DELETE /api/shell/:id`
pub async fn remove(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let _resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    manager().remove(&id).await.map_err(ErrorResponse::from)?;
    Ok(no_content())
}

#[derive(Debug, Deserialize)]
pub(crate) struct SessionShellBody {
    #[serde(default)]
    id: Option<String>,
    command: String,
}

/// `POST /api/session/:sid/shell`
pub(crate) async fn session_shell(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
    body: Result<Json<SessionShellBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let session = store
        .get_session(&sid)
        .await?
        .ok_or_else(|| session_not_found(&sid))?;
    let cwd = session
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .map_or_else(|| state.directory().to_path_buf(), PathBuf::from);
    let mut metadata = Map::new();
    metadata.insert("sessionID".to_owned(), json!(sid));
    metadata.insert("background".to_owned(), json!(true));
    if let Some(id) = payload.id {
        metadata.insert("messageID".to_owned(), json!(id));
    }
    let location = EventLocation {
        directory: cwd.to_string_lossy().into_owned(),
    };
    let location_directory = Some(location.directory.clone());
    let mut env = BTreeMap::new();
    if let Some(variables) = session
        .get("metadata")
        .and_then(|metadata| metadata.get("environment"))
        .and_then(Value::as_object)
    {
        for (key, value) in variables {
            if let Some(value) = value.as_str() {
                env.insert(key.clone(), value.to_owned());
            }
        }
    }
    env.insert("OPENCODE_SESSION_ID".to_owned(), sid);
    manager()
        .create(CreateShell {
            command: payload.command,
            cwd,
            shell: configured_shell(&state),
            env,
            timeout: None,
            metadata: Value::Object(metadata),
            bus: Some(state.bus().clone()),
            location: Some(location),
            location_directory,
        })
        .await
        .map_err(ErrorResponse::from)?;
    Ok(no_content())
}

fn configured_shell(state: &AppState) -> Option<String> {
    state.config().map(|config| config.shell())
}

fn metadata_object(metadata: Option<Value>) -> Result<Value, ErrorResponse> {
    match metadata {
        None | Some(Value::Null) => Ok(json!({})),
        Some(value @ Value::Object(_)) => Ok(value),
        Some(_) => Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "metadata must be an object",
        ))),
    }
}

fn parse_number(raw: Option<&str>, field: &str) -> Result<u64, ErrorResponse> {
    match raw {
        None => Ok(0),
        Some(raw) => raw.parse::<u64>().map_err(|_| {
            ErrorResponse::from(ApiError::new(
                400,
                "InvalidRequestError",
                format!("{field} must be a non-negative integer"),
            ))
        }),
    }
}

fn shell_not_found(id: &str) -> ApiError {
    ApiError::not_found(
        "ShellNotFoundError",
        format!("shell command not found: {id}"),
    )
    .with("id", json!(id))
}

// ---------------------------------------------------------------------------
// Persistent PTY aliases
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Binding {
    session_id: String,
}

static BINDINGS: LazyLock<Mutex<HashMap<String, Binding>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CURRENT: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PersistentCreate {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    cwd: Option<String>,
    title: String,
    env: BTreeMap<String, String>,
    size: Option<PtySize>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct PtySize {
    cols: u16,
    rows: u16,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PersistentUpdate {
    size: Option<PtySize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ReadParams {
    lines: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PersistentListParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// `GET /api/experimental/persistent-pty`
#[allow(clippy::needless_collect)] // the collect releases the bindings lock
pub(crate) async fn persistent_list(
    State(state): State<AppState>,
    params: Result<Query<PersistentListParams>, QueryRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_pty(&state)?;
    let bindings: Vec<(String, String)> = lock(&BINDINGS)
        .iter()
        .filter(|(_, binding)| {
            params
                .session_id
                .as_deref()
                .is_none_or(|session| binding.session_id == session)
        })
        .map(|(id, binding)| ((*id).clone(), binding.session_id.clone()))
        .collect();
    let mut terminals = Vec::new();
    for (id, session_id) in bindings {
        if let Some(info) = manager.info(&id) {
            terminals.push(persistent_info(&info, &session_id));
        }
    }
    Ok(Json(DataEnvelope::new(Value::Array(terminals))))
}

/// `POST /api/experimental/persistent-pty`
pub(crate) async fn persistent_create(
    State(state): State<AppState>,
    body: Result<Json<PersistentCreate>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let Some(session_id) = payload.session_id.filter(|value| !value.is_empty()) else {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "sessionID is required to create a persistent PTY",
        )));
    };
    let cwd = payload
        .cwd
        .filter(|value| !value.is_empty())
        .map_or_else(|| state.directory().to_path_buf(), PathBuf::from);
    let value = create_persistent(
        &state,
        &session_id,
        payload.command,
        payload.args,
        cwd,
        payload.title,
        payload.env,
        payload.size,
    )?;
    Ok(Json(DataEnvelope::new(value)))
}

/// `GET /api/experimental/session/:sid/terminal`
#[allow(clippy::needless_collect)] // the collect releases the bindings lock
pub async fn terminal_list(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let manager = require_pty(&state)?;
    let ids: Vec<String> = lock(&BINDINGS)
        .iter()
        .filter(|(_, binding)| binding.session_id == sid)
        .map(|(id, _)| (*id).clone())
        .collect();
    let mut terminals = Vec::new();
    for id in ids {
        if let Some(info) = manager.info(&id) {
            terminals.push(persistent_info(&info, &sid));
        }
    }
    Ok(Json(DataEnvelope::new(json!(terminals))))
}

/// `POST /api/experimental/session/:sid/terminal`
pub(crate) async fn terminal_create(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
    body: Result<Json<PersistentCreate>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let cwd = match payload.cwd.filter(|value| !value.is_empty()) {
        Some(cwd) => PathBuf::from(cwd),
        None => session_directory(&state, &sid)
            .await
            .unwrap_or_else(|| state.directory().to_path_buf()),
    };
    let value = create_persistent(
        &state,
        &sid,
        payload.command,
        payload.args,
        cwd,
        payload.title,
        payload.env,
        payload.size,
    )?;
    Ok(Json(DataEnvelope::new(value)))
}

#[allow(clippy::too_many_arguments)]
fn create_persistent(
    state: &AppState,
    session_id: &str,
    command: Option<String>,
    args: Vec<String>,
    cwd: PathBuf,
    title: String,
    mut env: BTreeMap<String, String>,
    size: Option<PtySize>,
) -> Result<Value, ErrorResponse> {
    if !session_id.starts_with("ses") {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "sessionID must be a Session.ID",
        )));
    }
    let manager = require_pty(state)?;
    let (cols, rows) = size.map_or((80, 24), |size| (size.cols, size.rows));
    if cols == 0 || rows == 0 {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "pty size rows and cols must be positive",
        )));
    }
    env.insert("OPENCODE_SESSION_ID".to_owned(), session_id.to_owned());
    let info = manager
        .create(CreatePty {
            id: None,
            shell: command,
            args,
            cwd: cwd.clone(),
            location: Some(cwd),
            env,
            cols,
            rows,
            title: Some(title),
        })
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    lock(&BINDINGS).insert(
        info.id.clone(),
        Binding {
            session_id: session_id.to_owned(),
        },
    );
    lock(&CURRENT).insert(session_id.to_owned(), info.id.clone());
    let terminal = persistent_info(&info, session_id);
    let mut event = EventEnvelope::now(
        "persistent-pty.added",
        json!({ "sessionID": session_id, "terminal": terminal }),
    );
    event.location = Some(EventLocation {
        directory: state.directory().to_string_lossy().into_owned(),
    });
    state.publish(event);
    Ok(terminal)
}

/// `GET /api/experimental/persistent-pty/:id`
pub async fn persistent_get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    Ok(Json(DataEnvelope::new(persistent_info(&info, &session_id))))
}

/// `PUT /api/experimental/persistent-pty/:id`
pub(crate) async fn persistent_update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<PersistentUpdate>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let Some(size) = payload.size else {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "size is required",
        )));
    };
    if size.cols == 0 || size.rows == 0 {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "pty size rows and cols must be positive",
        )));
    }
    manager
        .resize(&id, size.cols, size.rows)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    lock(&CURRENT).insert(session_id.clone(), id.clone());
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    Ok(Json(DataEnvelope::new(persistent_info(&info, &session_id))))
}

/// `DELETE /api/experimental/persistent-pty/:id`
pub async fn persistent_remove(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ErrorResponse> {
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    if info.running {
        let mut events = manager.subscribe();
        manager
            .terminate(&id)
            .map_err(|error| ErrorResponse::from(pty_error(error)))?;
        let _ = timeout(EXIT_WAIT, async {
            loop {
                match events.recv().await {
                    Ok(oc_pty::PtyEvent::Exited { id: event_id, .. }) if event_id == id => break,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
    }
    manager
        .remove(&id)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    lock(&BINDINGS).remove(&id);
    let mut current = lock(&CURRENT);
    if current.get(&session_id).is_some_and(|value| value == &id) {
        current.remove(&session_id);
    }
    drop(current);
    let mut event = EventEnvelope::now(
        "persistent-pty.removed",
        json!({ "sessionID": session_id, "ptyID": id }),
    );
    event.location = Some(EventLocation {
        directory: state.directory().to_string_lossy().into_owned(),
    });
    state.publish(event);
    Ok(no_content())
}

/// `POST /api/experimental/persistent-pty/:id/connect-token`
pub async fn persistent_connect_token(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    if headers
        .get(TICKET_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ErrorResponse::from(ApiError::new(
            403,
            "ForbiddenError",
            "Invalid persistent PTY connect token request",
        )));
    }
    let manager = require_pty(&state)?;
    binding(&id)?;
    if manager.info(&id).is_none() {
        return Err(ErrorResponse::from(pty_not_found(&id)));
    }
    let resolved = location::resolve(&state, &headers, None, None);
    let (ticket, expires_in) = state.tickets().issue(&id, &resolved.directory);
    Ok(Json(DataEnvelope::new(
        json!({ "ticket": ticket, "expires_in": expires_in }),
    )))
}

/// `GET /api/experimental/persistent-pty/:id/snapshot`
pub async fn persistent_snapshot(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    let text = manager
        .snapshot(&id, usize::from(info.rows))
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    let replay = manager
        .replay(&id, None)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    let cursor = end_cursor(&text, info.cols, info.rows);
    Ok(Json(DataEnvelope::new(json!({
        "info": persistent_info(&info, &session_id),
        "text": text,
        "checkpoint": BASE64.encode(replay.bytes.as_ref()),
        "cursor": cursor,
    }))))
}

/// `GET /api/experimental/session/:sid/terminal/read`
pub(crate) async fn terminal_read(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
    params: Result<Query<ReadParams>, QueryRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let lines = match params.lines.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<usize>() {
            Ok(lines) if (1..=65_535).contains(&lines) => Some(lines),
            _ => {
                return Err(ErrorResponse::from(ApiError::service_unavailable(
                    "lines must be an integer between 1 and 65535",
                )));
            }
        },
    };
    let manager = require_pty(&state)?;
    let Some(id) = lock(&CURRENT).get(&sid).cloned() else {
        return Ok(Json(DataEnvelope::new(Value::Null)));
    };
    let Some(binding) = lock(&BINDINGS).get(&id).cloned() else {
        lock(&CURRENT).remove(&sid);
        return Ok(Json(DataEnvelope::new(Value::Null)));
    };
    if binding.session_id != sid {
        lock(&CURRENT).remove(&sid);
        return Ok(Json(DataEnvelope::new(Value::Null)));
    }
    let Some(info) = manager.info(&id) else {
        lock(&CURRENT).remove(&sid);
        return Ok(Json(DataEnvelope::new(Value::Null)));
    };
    let rows = lines.unwrap_or(usize::from(info.rows));
    let text = manager
        .snapshot(&id, rows)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    let cursor = end_cursor(&text, info.cols, info.rows);
    Ok(Json(DataEnvelope::new(json!({
        "ptyID": info.id,
        "title": info.title.clone().unwrap_or_default(),
        "cwd": info.cwd.to_string_lossy().into_owned(),
        "foregroundProcess": Value::Null,
        "screen": {
            "text": text,
            "cols": u32::from(info.cols.max(1)),
            "rows": u32::from(info.rows.max(1)),
            "cursor": cursor,
        },
    }))))
}

fn persistent_info(info: &PtyInfo, session_id: &str) -> Value {
    let mut value = Map::new();
    value.insert("id".to_owned(), json!(info.id));
    value.insert(
        "title".to_owned(),
        json!(info.title.clone().unwrap_or_default()),
    );
    value.insert("command".to_owned(), json!(info.command));
    value.insert("args".to_owned(), json!(info.args));
    value.insert(
        "cwd".to_owned(),
        json!(info.cwd.to_string_lossy().into_owned()),
    );
    value.insert(
        "status".to_owned(),
        json!(if info.running { "running" } else { "exited" }),
    );
    value.insert("pid".to_owned(), json!(info.pid.unwrap_or(0)));
    if let Some(exit) = info.exit.filter(|exit| *exit >= 0) {
        value.insert("exitCode".to_owned(), json!(exit));
    }
    value.insert("sessionID".to_owned(), json!(session_id));
    value.insert("foregroundProcess".to_owned(), Value::Null);
    value.insert(
        "size".to_owned(),
        json!({ "cols": info.cols, "rows": info.rows }),
    );
    value.insert(
        "output".to_owned(),
        json!({ "head": info.output_head, "tail": info.output_bytes }),
    );
    Value::Object(value)
}

/// The wire cursor for the retained text: the position just past the last
/// printable character of the final row. `oc-pty` stores bytes, not cells, so
/// cursor-addressing sequences are not replayed here.
fn end_cursor(text: &str, cols: u16, rows: u16) -> Value {
    let cols = u32::from(cols.max(1));
    let rows = u32::from(rows.max(1));
    let lines: Vec<&str> = text.split('\n').collect();
    let y = u32::try_from(lines.len().saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(rows - 1);
    let width = lines.last().map_or(0, |line| {
        u32::try_from(line.chars().count()).unwrap_or(u32::MAX)
    });
    json!({ "x": width.min(cols - 1), "y": y })
}

fn binding(id: &str) -> Result<String, ErrorResponse> {
    lock(&BINDINGS)
        .get(id)
        .map(|binding| binding.session_id.clone())
        .ok_or_else(|| ErrorResponse::from(pty_not_found(id)))
}

async fn session_directory(state: &AppState, session_id: &str) -> Option<PathBuf> {
    let store = state.store()?;
    let session = store.get_session(session_id).await.ok()??;
    session
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn require_pty(state: &AppState) -> Result<&Arc<PtyManager>, ErrorResponse> {
    state.pty().ok_or_else(|| {
        ErrorResponse::from(ApiError::service_unavailable("pty manager not available"))
    })
}

fn pty_not_found(id: &str) -> ApiError {
    ApiError::not_found("PtyNotFoundError", format!("PTY session not found: {id}"))
        .with("ptyID", json!(id))
}

fn pty_error(error: PtyError) -> ApiError {
    match error {
        PtyError::NotFound { id } => pty_not_found(&id),
        PtyError::Conflict { id } => {
            ApiError::conflict(format!("PTY session already exists: {id}"))
        }
        PtyError::Limit { limit } => {
            ApiError::conflict(format!("live terminal limit reached: {limit}"))
        }
        PtyError::Running { id } => ApiError::conflict(format!("PTY session is running: {id}")),
        PtyError::Exited { id } => ApiError::conflict(format!("PTY session has exited: {id}")),
        PtyError::RowGap { requested, oldest } => ApiError::bad_request(format!(
            "row {requested} is no longer retained (oldest retained row is {oldest})"
        )),
        PtyError::Operation { operation, message } => {
            ApiError::bad_request(format!("pty {operation} failed: {message}"))
        }
        PtyError::Shutdown => ApiError::service_unavailable("pty manager is shut down"),
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    fn pages_advance_the_absolute_cursor() {
        let mut output = Output::new(1024);
        output.append(b"abcdefghij");
        assert_eq!(output.read(0, 4), ("abcd".to_owned(), 4, false));
        assert_eq!(output.read(4, 4), ("efgh".to_owned(), 8, false));
        assert_eq!(output.read(8, 4), ("ij".to_owned(), 10, false));
        assert_eq!(output.read(10, 4), (String::new(), 10, false));
        assert_eq!(output.read(99, 4), (String::new(), 10, false));
        assert_eq!(output.read(0, 0), (String::new(), 0, false));
    }

    #[test]
    fn evicted_cursors_are_clamped_and_reported_truncated() {
        let mut output = Output::new(8);
        output.append(b"abcdefgh");
        output.append(b"ij");
        assert_eq!(output.size, 10);
        assert_eq!(output.start, 2);
        assert_eq!(output.read(0, 4), ("cdef".to_owned(), 6, true));
        assert_eq!(output.read(6, 4), ("ghij".to_owned(), 10, false));
        // Re-reading the oldest retained byte is no longer truncated.
        assert_eq!(output.read(2, 2), ("cd".to_owned(), 4, false));
    }

    #[test]
    fn utf8_continuation_bytes_are_not_split_at_the_front() {
        let mut output = Output::new(3);
        output.append("aé".as_bytes());
        output.append("z".as_bytes());
        // The window dropped `a`; it starts on the intact `é`.
        let (text, _, truncated) = output.read(0, 16);
        assert!(truncated);
        assert_eq!(text, "éz");
    }
}
