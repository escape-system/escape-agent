//! [`PtyManager`]: terminal registry, creation, limits and shutdown.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::broadcast;

use crate::shell;
use crate::terminal::Terminal;
use crate::{CreatePty, PtyConfig, PtyError, PtyEvent, PtyInfo, PtyOutput, TERMINATE_GRACE};

/// Capacity of the event channel, in frames. A slow subscriber observes
/// `RecvError::Lagged` and must resynchronize with [`PtyManager::replay`].
const EVENT_CAPACITY: usize = 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failure(operation: &'static str, error: impl fmt::Display) -> PtyError {
    PtyError::Operation {
        operation,
        message: error.to_string(),
    }
}

fn not_found(id: &str) -> PtyError {
    PtyError::NotFound { id: id.to_string() }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

struct State {
    terminals: HashMap<String, Arc<Terminal>>,
    /// Ids of exited terminals in exit order (oldest first).
    exited: VecDeque<String>,
}

/// State shared by every clone of a [`PtyManager`].
pub(crate) struct Shared {
    pub(crate) config: PtyConfig,
    pub(crate) events: broadcast::Sender<PtyEvent>,
    state: Mutex<State>,
    shutting_down: AtomicBool,
    next_id: AtomicU64,
}

impl Shared {
    /// Records an exit for retention/eviction. Called by the exit watcher.
    pub(crate) fn retire(&self, id: &str) {
        if self.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let mut state = lock(&self.state);
        if !state.terminals.contains_key(id) {
            return;
        }
        if !state.exited.iter().any(|existing| existing == id) {
            state.exited.push_back(id.to_string());
        }
        while state.exited.len() > self.config.retained_exited {
            let Some(oldest) = state.exited.pop_front() else {
                break;
            };
            state.terminals.remove(&oldest);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            for terminal in state.terminals.values() {
                terminal.kill_now();
            }
        }
    }
}

/// In-process terminal manager.
///
/// The handle is cheap to clone (`Arc` inside) and safe to share across
/// tasks and threads. All methods are synchronous; terminal I/O and exit
/// watching run on dedicated OS threads, and events are delivered through a
/// Tokio broadcast channel.
#[derive(Clone)]
pub struct PtyManager {
    shared: Arc<Shared>,
}

impl fmt::Debug for PtyManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.shared.state);
        f.debug_struct("PtyManager")
            .field("config", &self.shared.config)
            .field("terminals", &state.terminals.len())
            .field("exited", &state.exited.len())
            .finish_non_exhaustive()
    }
}

impl PtyManager {
    pub fn new(config: PtyConfig) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            shared: Arc::new(Shared {
                config,
                events,
                state: Mutex::new(State {
                    terminals: HashMap::new(),
                    exited: VecDeque::new(),
                }),
                shutting_down: AtomicBool::new(false),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    pub fn config(&self) -> PtyConfig {
        self.shared.config
    }

    /// Spawns a terminal and returns its info.
    // The by-value request is part of the frozen crate API (docs/17).
    #[allow(clippy::needless_pass_by_value)]
    pub fn create(&self, req: CreatePty) -> Result<PtyInfo, PtyError> {
        if self.shared.shutting_down.load(Ordering::SeqCst) {
            return Err(PtyError::Shutdown);
        }
        let id = match req.id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => self.generate_id(),
        };
        let mut state = lock(&self.shared.state);
        if self.shared.shutting_down.load(Ordering::SeqCst) {
            return Err(PtyError::Shutdown);
        }
        if state.terminals.contains_key(&id) {
            return Err(PtyError::Conflict { id });
        }
        let live = state
            .terminals
            .values()
            .filter(|terminal| terminal.is_running())
            .count();
        if live >= self.shared.config.max_live {
            return Err(PtyError::Limit {
                limit: self.shared.config.max_live,
            });
        }
        let terminal = self.spawn_terminal(&id, &req)?;
        let info = terminal.info();
        state.terminals.insert(id, terminal);
        Ok(info)
    }

    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), PtyError> {
        self.terminal(id)?.write(data)
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), PtyError> {
        self.terminal(id)?.resize(cols, rows)
    }

    /// Replaces the terminal title and returns the updated info.
    pub fn set_title(&self, id: &str, title: &str) -> Result<PtyInfo, PtyError> {
        let terminal = self.terminal(id)?;
        terminal.rename(title.to_owned());
        Ok(terminal.info())
    }

    /// Last `rows` logical lines, ANSI CSI/OSC/DCS/ESC sequences stripped and
    /// `\r` normalized (`\r\n` → `\n`, a lone `\r` → `\n`). The result has no
    /// trailing newline; `rows == 0` is empty.
    pub fn snapshot(&self, id: &str, rows: usize) -> Result<String, PtyError> {
        Ok(self.terminal(id)?.snapshot(rows))
    }

    /// Rows `[start, start + count)` by absolute index.
    ///
    /// Row 0 starts at the first byte ever produced and the index increments
    /// on every `\n`; the final partial line is a row. Rows are raw text (no
    /// ANSI stripping). When the ring evicts bytes the oldest row index
    /// advances, the first retained row may be a partial line, and requesting
    /// an evicted row returns [`PtyError::RowGap`] with the oldest index.
    pub fn read_rows(&self, id: &str, start: u64, count: usize) -> Result<Vec<String>, PtyError> {
        self.terminal(id)?.rows(start, count)
    }

    /// Retained output starting at an absolute byte cursor (`None` = oldest
    /// retained byte). The returned cursor is the attach point for live
    /// [`PtyEvent::Output`] frames; `Some(u64::MAX)` tails the stream.
    pub fn replay(&self, id: &str, cursor: Option<u64>) -> Result<PtyOutput, PtyError> {
        Ok(self.terminal(id)?.replay(cursor))
    }

    pub fn info(&self, id: &str) -> Option<PtyInfo> {
        lock(&self.shared.state)
            .terminals
            .get(id)
            .map(|terminal| terminal.info())
    }

    pub fn list(&self) -> Vec<PtyInfo> {
        let terminals: Vec<Arc<Terminal>> = lock(&self.shared.state)
            .terminals
            .values()
            .cloned()
            .collect();
        let mut infos: Vec<PtyInfo> = terminals.iter().map(|terminal| terminal.info()).collect();
        infos.sort_by(|left, right| {
            left.created_ms
                .cmp(&right.created_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        infos
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PtyEvent> {
        self.shared.events.subscribe()
    }

    /// Sends the kill signal; the exit is observed asynchronously (and forced
    /// after three seconds if the child ignores it).
    pub fn terminate(&self, id: &str) -> Result<(), PtyError> {
        self.terminal(id)?.request_terminate();
        Ok(())
    }

    /// Deletes an exited terminal. Call [`PtyManager::terminate`] first for a
    /// running one.
    pub fn remove(&self, id: &str) -> Result<(), PtyError> {
        let terminal = {
            let mut state = lock(&self.shared.state);
            let terminal = state
                .terminals
                .get(id)
                .cloned()
                .ok_or_else(|| not_found(id))?;
            if terminal.is_running() {
                return Err(PtyError::Running { id: id.to_string() });
            }
            state.terminals.remove(id);
            state.exited.retain(|existing| existing != id);
            terminal
        };
        terminal.close(TERMINATE_GRACE);
        Ok(())
    }

    /// Terminates every terminal, joins its reader/watcher threads and empties
    /// the registry. Subsequent `create` calls fail with
    /// [`PtyError::Shutdown`].
    pub fn shutdown(&self) {
        if self.shared.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        let terminals: Vec<Arc<Terminal>> = {
            let mut state = lock(&self.shared.state);
            state.exited.clear();
            state
                .terminals
                .drain()
                .map(|(_, terminal)| terminal)
                .collect()
        };
        for terminal in &terminals {
            terminal.request_terminate();
        }
        let deadline = Instant::now() + TERMINATE_GRACE;
        for terminal in &terminals {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !terminal.wait_exited(remaining) {
                terminal.mark_exited(None);
            }
        }
        let deadline = Instant::now() + TERMINATE_GRACE;
        for terminal in &terminals {
            let remaining = deadline.saturating_duration_since(Instant::now());
            terminal.close(remaining);
        }
    }

    fn terminal(&self, id: &str) -> Result<Arc<Terminal>, PtyError> {
        lock(&self.shared.state)
            .terminals
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(id))
    }

    fn generate_id(&self) -> String {
        let sequence = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        format!("pty_{:x}{sequence:04x}", now_ms().max(0))
    }

    fn spawn_terminal(&self, id: &str, req: &CreatePty) -> Result<Arc<Terminal>, PtyError> {
        let program = shell::resolve(req.shell.as_deref());
        let args = if req.args.is_empty() {
            shell::default_args(&program)
        } else {
            req.args.clone()
        };
        let cwd = if req.cwd.as_os_str().is_empty() {
            std::env::current_dir().map_err(|error| failure("cwd", error))?
        } else {
            req.cwd.clone()
        };
        let cols = if req.cols == 0 { 80 } else { req.cols };
        let rows = if req.rows == 0 { 24 } else { req.rows };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| failure("openpty", error))?;

        let mut command = CommandBuilder::new(&program);
        for arg in &args {
            command.arg(arg);
        }
        command.cwd(&cwd);
        for (key, value) in &req.env {
            command.env(key, value);
        }
        if !req.env.contains_key("TERM") {
            command.env("TERM", "xterm-256color");
        }

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| failure("spawn", error))?;
        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| failure("reader", error))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| failure("writer", error))?;
        let master = pair.master;
        drop(pair.slave);

        let terminal = Arc::new(Terminal::new(
            id.to_string(),
            now_ms(),
            program.clone(),
            args.clone(),
            cwd.clone(),
            req.location.clone(),
            pid,
            (cols, rows),
            req.title.clone(),
            self.shared.config.buffer_bytes,
            Arc::downgrade(&self.shared),
        ));
        terminal.attach_io(master, writer, killer);
        if let Err(error) = terminal.start_watcher(child) {
            terminal.kill_now();
            return Err(failure("watcher", error));
        }
        if let Err(error) = terminal.start_reader(reader) {
            terminal.kill_now();
            return Err(failure("reader", error));
        }
        Ok(terminal)
    }
}
