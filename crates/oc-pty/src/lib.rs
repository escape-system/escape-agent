//! In-process PTY manager for opencode-rust.
//!
//! `oc-pty` owns real pseudo-terminals through [`portable_pty`]. One
//! [`PtyManager`] can host up to [`PtyConfig::max_live`] running terminals and
//! keeps a bounded scrollback per terminal. It deliberately contains no
//! terminal cell model: bytes are retained raw, and the only rendering helpers
//! are [`PtyManager::snapshot`] (last N lines with ANSI CSI/OSC/ESC sequences
//! removed) and [`PtyManager::read_rows`] (absolute row access).
//!
//! # Wire mapping (upstream `/api/pty`)
//!
//! - `GET /api/pty` → [`PtyManager::list`]
//! - `POST /api/pty` → [`PtyManager::create`] (`command` maps to
//!   [`CreatePty::shell`], `title`, `env`, `args`, `cwd`)
//! - `GET /api/pty/:id` → [`PtyManager::info`]
//! - `PUT /api/pty/:id` → [`PtyManager::resize`] (`size`) and/or
//!   [`PtyManager::set_title`]; OSC 0/1/2 output can replace the title later
//! - `DELETE /api/pty/:id` → [`PtyManager::terminate`] then
//!   [`PtyManager::remove`] when the exit event arrives
//! - `GET /api/pty/:id/connect` → [`PtyManager::subscribe`] for live
//!   [`PtyEvent::Output`], [`PtyManager::replay`] for the handshake backlog,
//!   [`PtyManager::write`] for client input, [`PtyManager::resize`] for
//!   viewport changes.
//!
//! Upstream's WebSocket contract is: outbound frames are raw UTF-8 terminal
//! chunks followed by one control frame (`0x00` + JSON `{"cursor":<n>}`) after
//! replay; `cursor` is an absolute byte offset into the terminal's output
//! stream. [`PtyInfo::output_bytes`] and [`PtyOutput::cursor`] are that
//! offset. Inbound frames are UTF-8 text (invalid UTF-8 is dropped by the
//! server) written verbatim to the PTY master.

mod buffer;
mod manager;
mod shell;
mod terminal;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;

pub use manager::PtyManager;

/// How long `terminate`/`shutdown` wait for a child to die before marking it
/// exited anyway.
pub(crate) const TERMINATE_GRACE: Duration = Duration::from_secs(3);

/// Resource limits for a [`PtyManager`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtyConfig {
    /// Maximum number of simultaneously running terminals. Creating another
    /// one fails with [`PtyError::Limit`].
    pub max_live: usize,
    /// Number of exited terminals kept observable until evicted. `0` removes
    /// them as soon as they exit.
    pub retained_exited: usize,
    /// Scrollback retained per terminal, in bytes. Oldest bytes are dropped
    /// first.
    pub buffer_bytes: usize,
    /// Suggestion for callers that render a default viewport
    /// (`snapshot`/`read_rows` use their explicit argument).
    pub read_rows_default: usize,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            max_live: 32,
            retained_exited: 16,
            buffer_bytes: 1024 * 1024,
            read_rows_default: 100,
        }
    }
}

/// Request to create a terminal.
///
/// `shell` defaults to the platform shell; `args` replaces that shell's
/// default arguments when non-empty. No string is ever interpreted by a
/// shell: `shell`, `args` and `env` are passed to the OS spawn call verbatim.
#[derive(Debug, Clone)]
pub struct CreatePty {
    /// Caller-supplied id (upstream `pty_*`). Generated when absent or empty.
    pub id: Option<String>,
    /// Program to run. `None` selects the platform default shell.
    pub shell: Option<String>,
    /// Arguments; when empty, login shells get `-l` (upstream parity) and
    /// other programs get none.
    pub args: Vec<String>,
    /// Working directory. Empty means the process cwd.
    pub cwd: PathBuf,
    /// Location directory that owns the terminal. HTTP callers use it to scope
    /// visibility; `None` means no location owns it.
    pub location: Option<PathBuf>,
    /// Extra environment; inherited from the server process otherwise.
    pub env: BTreeMap<String, String>,
    /// Viewport columns; `0` means 80.
    pub cols: u16,
    /// Viewport rows; `0` means 24.
    pub rows: u16,
    /// Initial title (OSC title reports can replace it).
    pub title: Option<String>,
}

/// Wire-shaped info for one terminal (the `Pty` payload plus retained-output
/// bounds used by the persistent alias).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyInfo {
    pub id: String,
    pub title: Option<String>,
    /// Resolved program that was spawned.
    pub command: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Working directory the child runs in.
    pub cwd: PathBuf,
    /// Location directory that owns the terminal (see [`CreatePty::location`]).
    pub location: Option<PathBuf>,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub running: bool,
    /// Exit code once exited; `None` while running or when the OS reported no
    /// code.
    pub exit: Option<i32>,
    /// Creation time, epoch milliseconds.
    pub created_ms: i64,
    /// Absolute offset of the oldest retained output byte.
    pub output_head: u64,
    /// Total bytes ever produced (monotonic even after ring eviction).
    pub output_bytes: u64,
}

/// Event published to [`PtyManager::subscribe`] receivers.
#[derive(Debug, Clone)]
pub enum PtyEvent {
    /// Raw output as read from the master side.
    Output { id: String, bytes: Bytes },
    /// Emitted exactly once per terminal.
    Exited { id: String, exit: Option<i32> },
    /// Terminal title reported through OSC 0/1/2.
    Title { id: String, title: String },
}

/// Retained output from [`PtyManager::replay`].
#[derive(Debug, Clone)]
pub struct PtyOutput {
    /// Bytes currently retained starting at the oldest available cursor.
    pub bytes: Bytes,
    /// Absolute output cursor after `bytes`; pass this back as `cursor` after
    /// the client applies the backlog.
    pub cursor: u64,
}

/// Errors returned by [`PtyManager`].
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("terminal `{id}` not found")]
    NotFound { id: String },
    #[error("terminal `{id}` already exists")]
    Conflict { id: String },
    #[error("live terminal limit ({limit}) reached")]
    Limit { limit: usize },
    #[error("terminal `{id}` is still running")]
    Running { id: String },
    #[error("terminal `{id}` has exited")]
    Exited { id: String },
    #[error("row {requested} is no longer retained (oldest retained row is {oldest})")]
    RowGap { requested: u64, oldest: u64 },
    #[error("pty {operation} failed: {message}")]
    Operation {
        operation: &'static str,
        message: String,
    },
    #[error("pty manager is shut down")]
    Shutdown,
}
