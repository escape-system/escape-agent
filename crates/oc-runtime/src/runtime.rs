//! Session actor, event sink and public runtime surface.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use oc_config::ConfigService;
use oc_llm::{Catalog, LlmError, LlmRequest, LlmStream, SharedLlmClient};
use oc_memory::{MemoryService, ScribeModel};
use oc_store::Store;
use oc_tools::ToolRegistry;
use oc_types::payload::EventPayload;
use oc_types::{
    Durable, EventEnvelope, EventLocation, InterruptReason, SessionId, SessionInfo, StructuredError,
};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::turn;

/// Sink that publishes protocol events. The server implements this over its SSE
/// bus; the runtime never depends on `oc-server`.
pub trait EventSink: Send + Sync {
    /// Publish one envelope. Must not block; the runtime calls it from the
    /// session actor.
    fn publish(&self, event: EventEnvelope);
}

/// A permission question raised while a tool is being authorised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAsk {
    pub session_id: String,
    pub message_id: String,
    pub tool_call_id: String,
    pub tool: String,
    pub action: String,
    pub resources: Vec<String>,
    /// Human-readable prompt for the client; optional.
    pub message: Option<String>,
}

/// Result of asking the host to resolve a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { message: String },
}

/// Host-provided permission resolver. `Ask` rules are handed to this handler;
/// `Allow`/`Deny` config rules never reach it.
pub trait PermissionHandler: Send + Sync {
    fn resolve(&self, request: PermissionAsk) -> BoxFuture<'static, PermissionDecision>;
}

/// Provider call surface used by the turn loop. The production implementation
/// is [`LlmClientAdapter`]; tests supply scripted implementations so no HTTP
/// request is ever made.
pub trait ModelClient: Send + Sync {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>>;
}

/// [`ModelClient`] over the real [`SharedLlmClient`].
#[derive(Debug, Clone)]
pub struct LlmClientAdapter {
    client: Arc<SharedLlmClient>,
}

impl LlmClientAdapter {
    #[must_use]
    pub fn new(client: Arc<SharedLlmClient>) -> Self {
        Self { client }
    }
}

impl ModelClient for LlmClientAdapter {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        let client = Arc::clone(&self.client);
        Box::pin(async move { client.0.stream(request).await })
    }
}

/// Host-supplied tool source. The runtime calls every provider once per
/// execution and merges the returned tools after the static registry, so a
/// provider may replace a built-in by name.
///
/// `session` is the resolved session row and `agent` the agent id that owns
/// the turn. Providers must be cheap and synchronous: the turn loop holds no
/// async lock across the call.
pub trait ToolProvider: Send + Sync {
    /// Tools visible to this session/agent combination.
    fn tools(&self, session: &SessionInfo, agent: &str) -> Vec<Arc<dyn oc_tools::Tool>>;
}

/// Construction inputs for [`Runtime`].
#[derive(Clone)]
pub struct RuntimeConfig {
    pub store: Arc<Store>,
    pub config: Arc<ConfigService>,
    pub catalog: Arc<Catalog>,
    pub client: Arc<SharedLlmClient>,
    pub tools: Arc<ToolRegistry>,
    pub sink: Arc<dyn EventSink>,
    pub permissions: Arc<dyn PermissionHandler>,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("store", &self.store)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

/// Failures the runtime reports to its host. Turn-level model failures are not
/// returned from [`Runtime::notify`]; they are persisted and published as
/// `session.execution.failed` so the actor can continue with the next prompt.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] oc_store::StoreError),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("invalid session: {0}")]
    InvalidSession(String),
    #[error("execution cancelled")]
    Cancelled,
    #[error("runtime is shut down")]
    Shutdown,
    #[error("timed out waiting for session {0} to become idle")]
    Timeout(String),
    #[error("task failed: {0}")]
    Task(String),
}

impl RuntimeError {
    /// Wire error used by assistant messages and terminal execution events.
    #[must_use]
    pub fn structured(&self) -> StructuredError {
        let kind = match self {
            Self::Store(_) | Self::Task(_) => "InternalError",
            Self::Llm(_) => "ProviderError",
            Self::Config(_) => "ConfigError",
            Self::SessionNotFound(_) | Self::InvalidSession(_) => "NotFoundError",
            Self::AgentNotFound(_) => "AgentNotFoundError",
            Self::ModelUnavailable(_) => "ModelNotFoundError",
            Self::Cancelled | Self::Shutdown => "MessageAbortedError",
            Self::Timeout(_) => "TimeoutError",
        };
        StructuredError::new(kind, self.to_string())
    }
}

/// Runtime handle. Cloning is cheap and shares the actor map.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("sessions", &self.active())
            .finish_non_exhaustive()
    }
}

pub(crate) struct Inner {
    pub(crate) config: RuntimeConfig,
    pub(crate) model: Arc<dyn ModelClient>,
    pub(crate) actors: Mutex<HashMap<String, Arc<ActorHandle>>>,
    pub(crate) observer: Mutex<Option<Arc<crate::observer::ObserverHost>>>,
    pub(crate) memory: Mutex<Option<Arc<MemoryService>>>,
    pub(crate) scribe: Mutex<Option<Arc<dyn ScribeModel>>>,
    pub(crate) tool_providers: Mutex<Vec<Arc<dyn ToolProvider>>>,
    pub(crate) lanes: Mutex<Option<Arc<oc_lanes::LaneService>>>,
    pub(crate) subagents: Mutex<crate::subagent::SubagentState>,
    pub(crate) shutdown: CancelToken,
    pub(crate) idle: Notify,
    pub(crate) started: AtomicBool,
}

pub(crate) struct ActorHandle {
    pub(crate) wake: Notify,
    pub(crate) idle: Notify,
    pub(crate) running: AtomicBool,
    /// Set by [`Runtime::request_background`] while an execution is running.
    /// The turn's foreground `subagent` wait consumes it and detaches; the
    /// actor resets it before every execution.
    pub(crate) background: AtomicBool,
    pub(crate) cancel: Mutex<Option<Arc<CancelToken>>>,
    pub(crate) join: Mutex<Option<JoinHandle<()>>>,
    /// Frozen instruction prefix for this session; deltas are computed against
    /// it so a turn never splices memory or skills per step.
    pub(crate) instructions: Mutex<Option<crate::instructions::InstructionSet>>,
}

impl ActorHandle {
    fn new() -> Self {
        Self {
            wake: Notify::new(),
            idle: Notify::new(),
            running: AtomicBool::new(false),
            background: AtomicBool::new(false),
            cancel: Mutex::new(None),
            join: Mutex::new(None),
            instructions: Mutex::new(None),
        }
    }
}

/// Cooperative cancellation token for one execution. The finalizer observes the
/// same token, so dropping a stream future cannot lose the terminal events.
pub(crate) struct CancelToken {
    cancelled: AtomicBool,
    reason: Mutex<InterruptReason>,
    notify: Notify,
    /// Shared with [`oc_tools::ToolContext`] so tools can stop helpers they
    /// spawned even after the tool future itself is dropped.
    pub(crate) token: tokio_util::sync::CancellationToken,
}

impl CancelToken {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            reason: Mutex::new(InterruptReason::User),
            notify: Notify::new(),
            token: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub(crate) fn cancel(&self, reason: InterruptReason) {
        *self.reason.lock().expect("cancel lock") = reason;
        self.cancelled.store(true, Ordering::SeqCst);
        self.token.cancel();
        self.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn reason(&self) -> InterruptReason {
        *self.reason.lock().expect("cancel lock")
    }

    pub(crate) async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl Runtime {
    /// Production constructor: `config.client` is adapted into a
    /// [`ModelClient`].
    #[must_use]
    pub fn new(config: RuntimeConfig) -> Self {
        let adapter = Arc::new(LlmClientAdapter::new(Arc::clone(&config.client)));
        Self::new_with_model_client(config, adapter)
    }

    /// Constructor for hosts that already own a [`ModelClient`], including
    /// tests with scripted offline models.
    #[must_use]
    pub fn new_with_model_client(config: RuntimeConfig, model: Arc<dyn ModelClient>) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                model,
                actors: Mutex::new(HashMap::new()),
                observer: Mutex::new(None),
                memory: Mutex::new(None),
                scribe: Mutex::new(None),
                tool_providers: Mutex::new(Vec::new()),
                lanes: Mutex::new(None),
                subagents: Mutex::new(crate::subagent::SubagentState::default()),
                shutdown: CancelToken::new(),
                idle: Notify::new(),
                started: AtomicBool::new(false),
            }),
        }
    }

    /// Attach permanent memory. The memory service must wrap the same store as
    /// [`RuntimeConfig::store`]; the runtime uses it for the `oc/memory`
    /// instruction source, the four memory tools and turn-end bookkeeping.
    #[must_use]
    pub fn with_memory(self, memory: Arc<MemoryService>) -> Self {
        *self.inner.memory.lock().expect("memory slot") = Some(memory);
        self
    }

    /// Attach the asynchronous memory scribe. The host adapts its model client
    /// to [`ScribeModel`]; the runtime only runs it after a settled successful
    /// turn and never blocks the response.
    #[must_use]
    pub fn with_scribe(self, scribe: Arc<dyn ScribeModel>) -> Self {
        *self.inner.scribe.lock().expect("scribe slot") = Some(scribe);
        self
    }

    /// The configured memory service, if any.
    #[must_use]
    pub fn memory(&self) -> Option<Arc<MemoryService>> {
        self.inner.memory.lock().expect("memory slot").clone()
    }

    pub(crate) fn scribe_model(&self) -> Option<Arc<dyn ScribeModel>> {
        self.inner.scribe.lock().expect("scribe slot").clone()
    }

    /// Attach a host tool provider (MCP, LSP, plugins, …). Providers are
    /// merged after [`RuntimeConfig::tools`] and before the runtime's own tool
    /// sets on every execution.
    pub fn add_tool_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.inner
            .tool_providers
            .lock()
            .expect("tool providers")
            .push(provider);
    }

    /// Tools returned by every attached provider for this session/agent.
    pub(crate) fn provider_tools(
        &self,
        session: &SessionInfo,
        agent: &str,
    ) -> Vec<Arc<dyn oc_tools::Tool>> {
        let providers: Vec<Arc<dyn ToolProvider>> = self
            .inner
            .tool_providers
            .lock()
            .expect("tool providers")
            .clone();
        providers
            .iter()
            .flat_map(|provider| provider.tools(session, agent))
            .collect()
    }

    /// Attach the build-lane scheduler. Lane tools are merged into every
    /// execution regardless; the service is what actually dispatches tickets.
    #[must_use]
    pub fn with_lanes(self, lanes: Arc<oc_lanes::LaneService>) -> Self {
        *self.inner.lanes.lock().expect("lanes slot") = Some(lanes);
        self
    }

    /// The attached lane service, when the host configured one.
    #[must_use]
    pub fn lanes(&self) -> Option<Arc<oc_lanes::LaneService>> {
        self.inner.lanes.lock().expect("lanes slot").clone()
    }

    /// Recover stale executions and wake sessions with deliverable inbox rows.
    ///
    /// Claims are process-local, so a fresh runtime already has no claims; the
    /// recovery pass repairs the durable rows a crashed process left behind.
    pub async fn start(&self) -> Result<(), RuntimeError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(RuntimeError::Shutdown);
        }
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let result = self.recover().await;
        if result.is_err() {
            self.inner.started.store(false, Ordering::SeqCst);
        } else {
            self.start_observer_worker();
        }
        result
    }

    /// Ensure the session has an actor and wake it. Admission belongs to the
    /// server: it writes the inbox row and user message before calling this.
    pub async fn notify(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.notify_local(session_id).await?;
        self.ensure_observer_ticker(session_id).await;
        Ok(())
    }

    /// Wake a session without starting an observer ticker.
    ///
    /// Observer-driven input uses this so the ticker future does not depend on
    /// itself through `notify`.
    pub(crate) async fn notify_local(&self, session_id: &str) -> Result<(), RuntimeError> {
        if self.inner.shutdown.is_cancelled() {
            return Err(RuntimeError::Shutdown);
        }
        if self
            .inner
            .config
            .store
            .get_session(session_id)
            .await?
            .is_none()
        {
            return Err(RuntimeError::SessionNotFound(session_id.to_owned()));
        }
        self.ensure_actor(session_id);
        Ok(())
    }

    /// Cancel the session's current execution. Returns `false` when idle.
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn interrupt(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return Ok(false);
        };
        let token = handle.cancel.lock().expect("cancel slot").clone();
        if let Some(token) = token {
            token.cancel(InterruptReason::User);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Mark the session's current execution as detached from the request.
    ///
    /// Returns `false` when the session has no running execution: there is
    /// nothing to detach, and the caller may still report success. The flag is
    /// consumed by the turn's foreground `subagent` wait (see
    /// [`crate::subagent`]); it never cancels anything.
    pub fn request_background(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return Ok(false);
        };
        if !handle.running.load(Ordering::SeqCst) {
            return Ok(false);
        }
        handle.background.store(true, Ordering::SeqCst);
        handle.wake.notify_one();
        Ok(true)
    }

    /// Consume a pending background request for `session`, clearing it so one
    /// request detaches one wait.
    pub(crate) fn take_background_request(&self, session: &str) -> bool {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.is_some_and(|handle| handle.background.swap(false, Ordering::SeqCst))
    }

    /// Wait until the session actor has no running execution and no deliverable
    /// inbox rows.
    pub async fn wait_idle(&self, session_id: &str, timeout: Duration) -> Result<(), RuntimeError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.is_idle(session_id).await? {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RuntimeError::Timeout(session_id.to_owned()));
            }
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_idle(session_id).await? {
                return Ok(());
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(RuntimeError::Timeout(session_id.to_owned()));
            }
        }
    }

    /// Sessions with a running execution, sorted for deterministic output.
    #[must_use]
    pub fn active(&self) -> Vec<String> {
        let actors = self.inner.actors.lock().expect("actor map");
        let mut active: Vec<String> = actors
            .iter()
            .filter(|(_, handle)| handle.running.load(Ordering::SeqCst))
            .map(|(session, _)| session.clone())
            .collect();
        active.sort();
        active
    }

    /// Cancel every execution, wait up to five seconds for finalizers, and drop
    /// the actor map. New [`notify`](Self::notify) calls fail afterwards.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel(InterruptReason::Shutdown);
        let handles: Vec<Arc<ActorHandle>> = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.values().cloned().collect()
        };
        for handle in &handles {
            if let Some(token) = handle.cancel.lock().expect("cancel slot").clone() {
                token.cancel(InterruptReason::Shutdown);
            }
            handle.wake.notify_one();
        }
        for handle in handles {
            let join = handle.join.lock().expect("join slot").take();
            if let Some(join) = join {
                let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
            }
        }
        self.inner.actors.lock().expect("actor map").clear();
        self.stop_observer();
    }

    /// Publish one typed event. Durable payloads are appended to the store
    /// first, and the envelope carries the allocated sequence number.
    pub(crate) async fn emit<P: EventPayload>(
        &self,
        session: &str,
        directory: &str,
        payload: P,
    ) -> Result<(), RuntimeError> {
        let data = payload.data();
        let mut envelope = EventEnvelope::now(P::TYPE, data.clone());
        envelope.location = Some(EventLocation {
            directory: directory.to_owned(),
        });
        if P::DURABLE {
            let created = envelope.created.unwrap_or_else(oc_types::now_millis);
            let seq = self
                .inner
                .config
                .store
                .append_event(oc_store::NewEvent {
                    id: envelope.id.as_str().to_owned(),
                    aggregate_id: session.to_owned(),
                    kind: P::TYPE.to_owned(),
                    data,
                    durable: true,
                    version: P::VERSION,
                    created,
                })
                .await?;
            envelope.durable = Some(Durable {
                aggregate_id: session.to_owned(),
                seq,
                version: P::VERSION,
            });
        }
        self.inner.config.sink.publish(envelope);
        Ok(())
    }

    pub(crate) fn session_id(session: &str) -> SessionId {
        SessionId::from_wire(session.to_owned())
    }

    fn ensure_actor(&self, session_id: &str) {
        let mut actors = self.inner.actors.lock().expect("actor map");
        if let Some(handle) = actors.get(session_id) {
            handle.wake.notify_one();
            return;
        }
        let handle = Arc::new(ActorHandle::new());
        actors.insert(session_id.to_owned(), Arc::clone(&handle));
        let runtime = self.clone();
        let session = session_id.to_owned();
        let actor = Arc::clone(&handle);
        let join = tokio::spawn(async move {
            runtime.actor_loop(session, actor).await;
        });
        *handle.join.lock().expect("join slot") = Some(join);
    }

    async fn actor_loop(&self, session: String, handle: Arc<ActorHandle>) {
        loop {
            if self.inner.shutdown.is_cancelled() {
                break;
            }
            handle.running.store(true, Ordering::SeqCst);
            let result = self.drain(&session, &handle).await;
            handle.running.store(false, Ordering::SeqCst);
            handle.idle.notify_waiters();
            self.inner.idle.notify_waiters();
            if let Err(error) = result {
                tracing::error!(session = %session, error = %error, "session actor stopped");
                break;
            }
            if self.inner.shutdown.is_cancelled() {
                break;
            }
            tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => break,
                () = handle.wake.notified() => {}
            }
        }
        handle.running.store(false, Ordering::SeqCst);
        handle.idle.notify_waiters();
        self.inner.idle.notify_waiters();
    }

    async fn drain(&self, session: &str, handle: &Arc<ActorHandle>) -> Result<(), RuntimeError> {
        loop {
            if self.inner.shutdown.is_cancelled() {
                return Ok(());
            }
            let Some(item) = self.next_deliverable(session).await? else {
                return Ok(());
            };
            let token = Arc::new(CancelToken::new());
            *handle.cancel.lock().expect("cancel slot") = Some(Arc::clone(&token));
            // A detach request applies to the execution that is about to run;
            // one left over from a previous item must never leak into it.
            handle.background.store(false, Ordering::SeqCst);
            let result = if item.get("type").and_then(Value::as_str) == Some("compaction") {
                self.run_manual_compaction(session, &item, &token).await
            } else {
                self.run_execution(session, &item, &token, handle).await
            };
            *handle.cancel.lock().expect("cancel slot") = None;
            result?;
        }
    }

    pub(crate) async fn is_idle(&self, session: &str) -> Result<bool, RuntimeError> {
        let running = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors
                .get(session)
                .is_some_and(|handle| handle.running.load(Ordering::SeqCst))
        };
        if running {
            return Ok(false);
        }
        Ok(self.next_deliverable(session).await?.is_none())
    }

    /// First deliverable inbox row, in enqueue order. `compaction` rows are
    /// delivered to the actor, which runs them itself; `move` is still skipped
    /// because no handler exists.
    pub(crate) async fn next_deliverable(
        &self,
        session: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let items = self.inner.config.store.list_inbox(session).await?;
        Ok(items.into_iter().find(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("user" | "synthetic" | "peer" | "observer" | "compaction")
            )
        }))
    }

    pub(crate) fn parse_session(value: &Value) -> Result<SessionInfo, RuntimeError> {
        serde_json::from_value(value.clone())
            .map_err(|error| RuntimeError::InvalidSession(error.to_string()))
    }

    /// All sessions, newest first, using keyset pagination.
    pub(crate) async fn list_all_sessions(&self) -> Result<Vec<Value>, RuntimeError> {
        let mut cursor = None;
        let mut out = Vec::new();
        loop {
            let page = self
                .inner
                .config
                .store
                .list_sessions(oc_store::SessionQuery {
                    limit: 200,
                    order: oc_store::Order::Desc,
                    cursor,
                    ..oc_store::SessionQuery::default()
                })
                .await?;
            out.extend(page.items);
            match page.cursor.next {
                Some(next) => cursor = Some(next),
                None => return Ok(out),
            }
        }
    }

    /// Every stored message for one session, oldest first.
    pub(crate) async fn list_all_messages(
        &self,
        session: &str,
    ) -> Result<Vec<Value>, RuntimeError> {
        let mut cursor = None;
        let mut out = Vec::new();
        loop {
            let page = self
                .inner
                .config
                .store
                .list_messages(oc_store::MessageQuery {
                    session_id: session.to_owned(),
                    limit: 200,
                    order: oc_store::Order::Asc,
                    cursor,
                    kind: None,
                })
                .await?;
            out.extend(page.items);
            match page.cursor.next {
                Some(next) => cursor = Some(next),
                None => return Ok(out),
            }
        }
    }

    pub(crate) async fn load_context(
        &self,
        session: &str,
    ) -> Result<Vec<oc_llm::LlmMessage>, RuntimeError> {
        let messages = self.list_all_messages(session).await?;
        Ok(crate::prompt::messages_from_store(&messages))
    }

    /// Internal turn entry point lives in `turn.rs`.
    async fn run_execution(
        &self,
        session: &str,
        item: &Value,
        cancel: &Arc<CancelToken>,
        handle: &Arc<ActorHandle>,
    ) -> Result<(), RuntimeError> {
        turn::run_execution(self, session, item, cancel, handle).await
    }
}
