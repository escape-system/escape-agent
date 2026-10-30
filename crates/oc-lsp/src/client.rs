//! One language-server process and its JSON-RPC session.

mod accessors;
mod diagnostics;
mod dispatch;
mod handshake;
mod io;
mod json;
mod lifecycle;
mod request;
mod spawn;
mod sync;
mod types;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex as SyncMutex;
use serde_json::Value;
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{broadcast, oneshot, watch};

use crate::LspEvent;
use crate::diagnostics::DiagnosticsCache;
use crate::rpc::{RequestId, RpcError};

pub use types::{
    DIAGNOSTICS_DEBOUNCE, DIAGNOSTICS_WAIT, INIT_TIMEOUT, MAX_FILE_BYTES, REQUEST_TIMEOUT,
    SpawnRequest,
};

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
