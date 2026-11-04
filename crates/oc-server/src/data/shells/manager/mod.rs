//! Shell registry, lifecycle and the process-global [`manager`] singleton.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use oc_types::{ApiError, EventEnvelope, EventLocation};
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio::sync::{Notify, broadcast};

use super::http::shell_not_found;
use super::selection::resolve_shell;
use super::util::{create_capture, lock, now_ms};
use super::{EXIT_WAIT, SHELL_BUFFER, SHELL_DIR};

mod output;
mod process;

use output::Output;
use process::{kill_tree, kill_tree_blocking, read_output, wait_finished, wait_for_exit};

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
        let args = kind.args(Path::new(&shell), &req.command);
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

    /// Whether `id` belongs to `directory`. A shell created without a location
    /// (only possible through the crate API) is owned by nobody over HTTP.
    #[must_use]
    pub fn owned_by(&self, id: &str, directory: &str) -> bool {
        lock(&self.registry)
            .shells
            .get(id)
            .is_some_and(|shell| shell.location_directory.as_deref() == Some(directory))
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
