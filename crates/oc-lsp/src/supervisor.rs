//! Supervisor: lazy, bounded, keyed language-server lifecycle.
//!
//! One client per `(server_id, canonical_root)`. Spawns are shared between
//! concurrent callers, failures back off exponentially, and idle clients are
//! evicted under a slot budget that charges heavy servers twice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::broadcast;

use crate::client::{DIAGNOSTICS_WAIT, INIT_TIMEOUT, LspClient, REQUEST_TIMEOUT, SpawnRequest};
use crate::config::{GLOBAL_SLOT_BUDGET, LspConfig, LspServerConfig, resolve_root};
use crate::error::LspError;
use crate::{LspEvent, tools};

/// Supervisor tuning knobs. Defaults follow docs/11-lsp.md.
#[derive(Debug, Clone)]
pub struct SupervisorOptions {
    /// Maximum concurrent clients.
    pub max_servers: usize,
    /// Global slot budget (heavy servers cost two).
    pub slot_budget: usize,
    /// `initialize` timeout.
    pub init_timeout: Duration,
    /// Idle clients are evicted after this long.
    pub idle_ttl: Duration,
    /// Sweep cadence.
    pub sweep_interval: Duration,
    /// Exponential backoff schedule for crashed servers.
    pub backoff: [Duration; 3],
    /// Per-request timeout for tool operations.
    pub request_timeout: Duration,
    /// Diagnostics wait budget after `didOpen`/`didChange`.
    pub diagnostics_wait: Duration,
    /// Start the background idle sweeper (requires a Tokio runtime).
    pub start_sweeper: bool,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            max_servers: 4,
            slot_budget: GLOBAL_SLOT_BUDGET,
            init_timeout: INIT_TIMEOUT,
            idle_ttl: Duration::from_secs(10 * 60),
            sweep_interval: Duration::from_secs(60),
            backoff: [
                Duration::from_secs(60),
                Duration::from_secs(5 * 60),
                Duration::from_secs(15 * 60),
            ],
            request_timeout: REQUEST_TIMEOUT,
            diagnostics_wait: DIAGNOSTICS_WAIT,
            start_sweeper: true,
        }
    }
}

/// `(server id, canonical root)` slot key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerKey {
    /// Server id (`rust`, `typescript`, …).
    pub server_id: String,
    /// Canonical project root the client serves.
    pub root: PathBuf,
}

/// Status row for `GET /api/lsp`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspStatus {
    /// Server id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Canonical root.
    pub root: String,
    /// `connected`, `starting` or `error`.
    pub status: &'static str,
    /// Resident set size when the platform exposes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
}

type SpawnFuture = Shared<BoxFuture<'static, Result<Arc<LspClient>, Arc<LspError>>>>;

#[derive(Debug)]
enum SlotState {
    Cold,
    Spawning(SpawnFuture),
    Ready(Arc<LspClient>),
    Broken {
        until: Instant,
        failures: u32,
        permanent: bool,
        reason: String,
    },
}

#[derive(Debug)]
struct Slot {
    key: ServerKey,
    name: String,
    cost: usize,
    failures: AtomicU32,
    state: AsyncMutex<SlotState>,
}

impl Slot {
    fn new(key: ServerKey, name: String, cost: usize) -> Self {
        Self {
            key,
            name,
            cost,
            failures: AtomicU32::new(0),
            state: AsyncMutex::new(SlotState::Cold),
        }
    }
}

#[derive(Debug, Default)]
struct Budget {
    clients: usize,
    slots: usize,
}

struct Inner {
    config: RwLock<LspConfig>,
    workspace_root: PathBuf,
    options: SupervisorOptions,
    max_servers: AtomicUsize,
    slots: Mutex<HashMap<ServerKey, Arc<Slot>>>,
    budget: Mutex<Budget>,
    events: broadcast::Sender<LspEvent>,
    shutting_down: AtomicBool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspSupervisorInner")
            .field("workspace_root", &self.workspace_root)
            .field("slots", &self.slots.lock().len())
            .finish_non_exhaustive()
    }
}

/// Keyed language-server supervisor.
#[derive(Clone)]
pub struct LspSupervisor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LspSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl LspSupervisor {
    /// Create a supervisor with configuration-derived `max_servers`.
    #[must_use]
    pub fn new(config: LspConfig, workspace_root: impl Into<PathBuf>) -> Self {
        let options = SupervisorOptions {
            max_servers: config.max_servers.max(1),
            ..SupervisorOptions::default()
        };
        Self::with_options(config, workspace_root, options)
    }

    /// Create a supervisor with explicit tuning (tests use short timeouts).
    #[must_use]
    pub fn with_options(
        mut config: LspConfig,
        workspace_root: impl Into<PathBuf>,
        options: SupervisorOptions,
    ) -> Self {
        let workspace_root = canonical_or_owned(&workspace_root.into());
        let max_servers = options.max_servers.max(1);
        config.max_servers = max_servers;
        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            config: RwLock::new(config),
            workspace_root,
            max_servers: AtomicUsize::new(max_servers),
            options,
            slots: Mutex::new(HashMap::new()),
            budget: Mutex::new(Budget::default()),
            events,
            shutting_down: AtomicBool::new(false),
        });
        if inner.options.start_sweeper {
            spawn_sweeper(Arc::downgrade(&inner), inner.options.sweep_interval);
        }
        Self { inner }
    }

    /// Subscribe to `lsp.updated` / `lsp.client.diagnostics` events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<LspEvent> {
        self.inner.events.subscribe()
    }

    /// Canonical workspace root.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.inner.workspace_root
    }

    /// Configuration snapshot.
    #[must_use]
    pub fn config(&self) -> LspConfig {
        self.inner.config.read().clone()
    }

    /// Whether LSP is enabled in configuration.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner.config.read().enabled
    }

    /// Replace configuration and drop every client.
    pub async fn reload(&self, config: LspConfig) {
        self.shutdown_all().await;
        self.inner
            .max_servers
            .store(config.max_servers.max(1), Ordering::Release);
        *self.inner.config.write() = config;
        self.inner.shutting_down.store(false, Ordering::Release);
        let _ = self.inner.events.send(LspEvent::Updated);
    }

    /// Restart every client with the supplied configuration (or the current
    /// snapshot when `None`).
    ///
    /// `shutdown_all` drains every slot, which clears the per-client
    /// diagnostics caches; the next file touch spawns a fresh client.
    pub async fn reload_config(&self, config: Option<LspConfig>) {
        let config = config.unwrap_or_else(|| self.config());
        self.reload(config).await;
    }

    /// Registry snapshot for `GET /api/lsp`.
    pub async fn statuses(&self) -> Vec<LspStatus> {
        let config = self.config();
        let slots: Vec<Arc<Slot>> = self.inner.slots.lock().values().cloned().collect();
        let mut statuses = Vec::with_capacity(slots.len());
        for slot in slots {
            let state = slot.state.lock().await;
            let status = match &*state {
                SlotState::Ready(client) if !client.is_exited() => "connected",
                SlotState::Ready(_) | SlotState::Broken { .. } => "error",
                SlotState::Spawning(_) | SlotState::Cold => "starting",
            };
            statuses.push(LspStatus {
                id: slot.key.server_id.clone(),
                name: config
                    .get(&slot.key.server_id)
                    .map_or_else(|| slot.name.clone(), |server| server.name.clone()),
                root: slot.key.root.to_string_lossy().into_owned(),
                status,
                rss_bytes: None,
            });
        }
        statuses.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.root.cmp(&right.root))
        });
        statuses
    }

    /// Return a ready client for `file`, spawning lazily.
    pub async fn client_for_file(&self, file: &Path) -> Result<Arc<LspClient>, LspError> {
        let config = self.config();
        if !config.enabled {
            return Err(LspError::Disabled);
        }
        let file = if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.inner.workspace_root.join(file)
        };
        let file = canonical_or_owned(&file);
        if !file.starts_with(&self.inner.workspace_root) {
            return Err(LspError::OutsideRoot(file));
        }
        let candidates: Vec<LspServerConfig> = config
            .servers_for_file(&file)
            .into_iter()
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Err(LspError::NoServer(file));
        }

        let mut unavailable: Option<LspError> = None;
        let mut failure: Option<LspError> = None;
        for server in candidates {
            let root = resolve_root(&server.id, &file, &self.inner.workspace_root);
            let argv = match server.resolve_command(&root) {
                Ok(argv) => argv,
                Err(reason) => {
                    unavailable.get_or_insert(LspError::Unavailable {
                        server: server.id.clone(),
                        reason,
                    });
                    continue;
                }
            };
            let key = ServerKey {
                server_id: server.id.clone(),
                root: root.clone(),
            };
            match self.ensure(&key, server, argv).await {
                Ok(client) => return Ok(client),
                Err(error) => {
                    if matches!(error, LspError::BudgetExceeded { .. }) {
                        failure = Some(error);
                    } else {
                        unavailable.get_or_insert(error);
                    }
                }
            }
        }
        Err(failure.or(unavailable).unwrap_or(LspError::NoServer(file)))
    }

    /// First ready client for the workspace root (used by `workspaceSymbol`
    /// when no file position was supplied).
    pub async fn client_for_workspace(&self) -> Result<Arc<LspClient>, LspError> {
        let config = self.config();
        if !config.enabled {
            return Err(LspError::Disabled);
        }
        let mut unavailable: Option<LspError> = None;
        let servers: Vec<LspServerConfig> = config.active_servers().into_iter().cloned().collect();
        for server in servers {
            let root = self.inner.workspace_root.clone();
            let argv = match server.resolve_command(&root) {
                Ok(argv) => argv,
                Err(reason) => {
                    unavailable.get_or_insert(LspError::Unavailable {
                        server: server.id.clone(),
                        reason,
                    });
                    continue;
                }
            };
            let key = ServerKey {
                server_id: server.id.clone(),
                root,
            };
            match self.ensure(&key, server, argv).await {
                Ok(client) => return Ok(client),
                Err(error) => {
                    unavailable.get_or_insert(error);
                }
            }
        }
        Err(unavailable.unwrap_or_else(|| LspError::NoServer(self.inner.workspace_root.clone())))
    }

    /// Open (or refresh) a file without waiting for diagnostics.
    pub async fn warm_file(&self, file: &Path) -> Result<(), LspError> {
        let client = self.client_for_file(file).await?;
        client.touch_file(file, false).await?;
        Ok(())
    }

    /// Synchronize a file after an edit and wait for diagnostics.
    pub async fn touch_and_wait(
        &self,
        file: &Path,
    ) -> Result<Option<crate::diagnostics::CachedDiagnostics>, LspError> {
        let client = self.client_for_file(file).await?;
        client.touch_file(file, true).await
    }

    /// Force a `didClose` for a file on every client that has it open.
    pub async fn close_file(&self, file: &Path) {
        let file = canonical_or_owned(file);
        let slots: Vec<Arc<Slot>> = self.inner.slots.lock().values().cloned().collect();
        for slot in slots {
            let client = {
                let state = slot.state.lock().await;
                match &*state {
                    SlotState::Ready(client) => Some(client.clone()),
                    _ => None,
                }
            };
            if let Some(client) = client {
                client.close_file(&file).await;
            }
        }
    }

    /// Cached diagnostics for a path, merging every client that has seen it.
    pub async fn diagnostics_json(&self, file: &Path) -> Value {
        let file = canonical_or_owned(file);
        let slots: Vec<Arc<Slot>> = self.inner.slots.lock().values().cloned().collect();
        let mut merged: Vec<Value> = Vec::new();
        for slot in slots {
            if !file.starts_with(&slot.key.root) {
                continue;
            }
            let cached = {
                let state = slot.state.lock().await;
                match &*state {
                    SlotState::Ready(client) => client.cached_diagnostics(&file),
                    _ => None,
                }
            };
            if let Some(cached) = cached {
                for item in cached.merged() {
                    if !merged.contains(&item) {
                        merged.push(item);
                    }
                }
            }
        }
        Value::Array(merged)
    }

    /// `lsp` tool entry point.
    pub async fn execute_tool(&self, input: &tools::LspToolInput) -> Result<Value, LspError> {
        tools::execute(self, input).await
    }

    /// `lsp_diagnostics` tool entry point.
    pub async fn execute_diagnostics_tool(&self, path: &Path) -> Result<Value, LspError> {
        Ok(self.diagnostics_json(path).await)
    }

    /// Shut every client down; used on reload and process exit.
    pub async fn shutdown_all(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        let slots: Vec<Arc<Slot>> = {
            let mut map = self.inner.slots.lock();
            map.drain().map(|(_, slot)| slot).collect()
        };
        let mut clients = Vec::new();
        for slot in &slots {
            let state = slot.state.lock().await;
            if let SlotState::Ready(client) = &*state {
                clients.push(client.clone());
            }
        }
        for client in clients {
            client.shutdown().await;
        }
        *self.inner.budget.lock() = Budget::default();
        let _ = self.inner.events.send(LspEvent::Updated);
    }

    async fn ensure(
        &self,
        key: &ServerKey,
        server: LspServerConfig,
        argv: Vec<String>,
    ) -> Result<Arc<LspClient>, LspError> {
        let slot = self.slot_for(key, &server);
        let action = self.action_for(&slot).await;
        let shared = match action {
            Action::Return(result) => return result,
            Action::Await(shared) => shared,
            Action::Start => self.begin_spawn(&slot, &server, argv, false).await,
            Action::Restart => self.begin_spawn(&slot, &server, argv, true).await,
        };
        self.finish_spawn(&slot, shared).await
    }

    fn slot_for(&self, key: &ServerKey, server: &LspServerConfig) -> Arc<Slot> {
        let mut slots = self.inner.slots.lock();
        Arc::clone(slots.entry(key.clone()).or_insert_with(|| {
            Arc::new(Slot::new(
                key.clone(),
                server.name.clone(),
                server.slot_cost(),
            ))
        }))
    }

    async fn action_for(&self, slot: &Arc<Slot>) -> Action {
        let state = slot.state.lock().await;
        match &*state {
            SlotState::Ready(client) if !client.is_exited() => {
                Action::Return(Ok(Arc::clone(client)))
            }
            SlotState::Ready(_) => Action::Restart,
            SlotState::Spawning(shared) => Action::Await(shared.clone()),
            SlotState::Broken {
                until,
                failures,
                permanent,
                reason,
            } => {
                if *permanent || Instant::now() < *until {
                    Action::Return(Err(LspError::Backoff {
                        server: slot.key.server_id.clone(),
                        failures: *failures,
                        reason: reason.clone(),
                    }))
                } else {
                    Action::Start
                }
            }
            SlotState::Cold => Action::Start,
        }
    }

    async fn begin_spawn(
        &self,
        slot: &Arc<Slot>,
        server: &LspServerConfig,
        argv: Vec<String>,
        restart: bool,
    ) -> SpawnFuture {
        if restart {
            let released = {
                let mut state = slot.state.lock().await;
                if matches!(&*state, SlotState::Ready(client) if client.is_exited()) {
                    *state = SlotState::Cold;
                    true
                } else {
                    false
                }
            };
            if released {
                self.inner.release(slot.cost);
            }
        }
        let started = self.start_spawn(server, slot.key.root.clone(), argv);
        let mut state = slot.state.lock().await;
        if let SlotState::Spawning(existing) = &*state {
            existing.clone()
        } else {
            *state = SlotState::Spawning(started.clone());
            started
        }
    }

    async fn finish_spawn(
        &self,
        slot: &Arc<Slot>,
        shared: SpawnFuture,
    ) -> Result<Arc<LspClient>, LspError> {
        match shared.await {
            Ok(client) => {
                let still_registered = {
                    let slots = self.inner.slots.lock();
                    slots
                        .get(&slot.key)
                        .is_some_and(|current| Arc::ptr_eq(current, slot))
                };
                if !still_registered || self.inner.shutting_down.load(Ordering::Acquire) {
                    client.shutdown().await;
                    return Err(LspError::Disabled);
                }
                *slot.state.lock().await = SlotState::Ready(Arc::clone(&client));
                slot.failures.store(0, Ordering::Release);
                let _ = self.inner.events.send(LspEvent::Updated);
                Ok(client)
            }
            Err(error) => {
                let attempt = slot.failures.fetch_add(1, Ordering::AcqRel) + 1;
                let index = usize::try_from(attempt - 1).unwrap_or(usize::MAX).min(2);
                let delay = self.inner.options.backoff[index];
                let permanent = error.is_permanent();
                *slot.state.lock().await = SlotState::Broken {
                    until: Instant::now() + delay,
                    failures: attempt,
                    permanent,
                    reason: error.to_string(),
                };
                let _ = self.inner.events.send(LspEvent::Updated);
                Err(error.duplicate())
            }
        }
    }

    fn start_spawn(
        &self,
        server: &LspServerConfig,
        root: PathBuf,
        argv: Vec<String>,
    ) -> SpawnFuture {
        let inner = Arc::clone(&self.inner);
        let server = server.clone();
        let cost = server.slot_cost();
        async move { inner.spawn_client(&server, root, argv, cost).await }
            .boxed()
            .shared()
    }

    async fn make_room(&self, cost: usize) -> Result<(), LspError> {
        loop {
            if self.inner.shutting_down.load(Ordering::Acquire) {
                return Err(LspError::Disabled);
            }
            let max_servers = self.inner.max_servers.load(Ordering::Acquire);
            {
                let mut budget = self.inner.budget.lock();
                if budget.clients < max_servers
                    && budget.slots + cost <= self.inner.options.slot_budget
                {
                    budget.clients += 1;
                    budget.slots += cost;
                    return Ok(());
                }
            }
            let Some((key, client, victim_cost)) = self.pick_victim().await else {
                return Err(LspError::BudgetExceeded {
                    max: max_servers,
                    slots: self.inner.options.slot_budget,
                });
            };
            self.evict_slot(key, client, victim_cost).await;
        }
    }

    async fn pick_victim(&self) -> Option<(ServerKey, Arc<LspClient>, usize)> {
        let slots: Vec<Arc<Slot>> = self.inner.slots.lock().values().cloned().collect();
        let mut best: Option<(bool, Instant, ServerKey, Arc<LspClient>, usize)> = None;
        for slot in slots {
            let state = slot.state.lock().await;
            let SlotState::Ready(client) = &*state else {
                continue;
            };
            if client.is_exited() || client.in_flight() > 0 {
                continue;
            }
            let heavy = slot.cost > 1;
            let used = client.last_used();
            let better = match &best {
                None => true,
                Some((best_heavy, best_used, ..)) => {
                    (heavy && !*best_heavy) || (heavy == *best_heavy && used < *best_used)
                }
            };
            if better {
                best = Some((heavy, used, slot.key.clone(), Arc::clone(client), slot.cost));
            }
        }
        best.map(|(_, _, key, client, cost)| (key, client, cost))
    }

    async fn evict_slot(&self, key: ServerKey, client: Arc<LspClient>, cost: usize) {
        if self.inner.slots.lock().remove(&key).is_none() {
            return;
        }
        client.shutdown().await;
        self.inner.release(cost);
        let _ = self.inner.events.send(LspEvent::Updated);
    }
}

enum Action {
    Return(Result<Arc<LspClient>, LspError>),
    Await(SpawnFuture),
    Start,
    Restart,
}

impl Inner {
    fn release(&self, cost: usize) {
        let mut budget = self.budget.lock();
        budget.clients = budget.clients.saturating_sub(1);
        budget.slots = budget.slots.saturating_sub(cost);
    }

    async fn spawn_client(
        self: &Arc<Self>,
        server: &LspServerConfig,
        root: PathBuf,
        argv: Vec<String>,
        cost: usize,
    ) -> Result<Arc<LspClient>, Arc<LspError>> {
        let supervisor = LspSupervisor {
            inner: Arc::clone(self),
        };
        supervisor.make_room(cost).await.map_err(Arc::new)?;
        let request = SpawnRequest {
            server: server.clone(),
            root,
            argv,
            events: self.events.clone(),
            init_timeout: self.options.init_timeout,
            request_timeout: self.options.request_timeout,
            diagnostics_wait: self.options.diagnostics_wait,
        };
        match LspClient::spawn(request).await {
            Ok(client) => {
                if self.shutting_down.load(Ordering::Acquire) {
                    client.shutdown().await;
                    self.release(cost);
                    return Err(Arc::new(LspError::Disabled));
                }
                Ok(Arc::new(client))
            }
            Err(error) => {
                self.release(cost);
                Err(Arc::new(error))
            }
        }
    }
}

async fn sweep(inner: &Arc<Inner>) {
    let ttl = inner.options.idle_ttl;
    let slots: Vec<Arc<Slot>> = inner.slots.lock().values().cloned().collect();
    for slot in slots {
        let expired = {
            let state = slot.state.lock().await;
            match &*state {
                SlotState::Ready(client) => {
                    client.in_flight() == 0 && client.last_used().elapsed() >= ttl
                }
                _ => false,
            }
        };
        if !expired {
            continue;
        }
        if inner.slots.lock().remove(&slot.key).is_none() {
            continue;
        }
        let client = {
            let state = slot.state.lock().await;
            match &*state {
                SlotState::Ready(client) => Some(Arc::clone(client)),
                _ => None,
            }
        };
        if let Some(client) = client {
            client.shutdown().await;
        }
        inner.release(slot.cost);
        let _ = inner.events.send(LspEvent::Updated);
    }
}

fn spawn_sweeper(inner: Weak<Inner>, interval: Duration) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                sweep(&inner).await;
            }
        });
    }
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
