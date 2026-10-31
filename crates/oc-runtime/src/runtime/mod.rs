//! Session actor, event sink and public runtime surface.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oc_memory::{MemoryService, ScribeModel};
use oc_types::payload::EventPayload;
use oc_types::{Durable, EventEnvelope, EventLocation, InterruptReason, SessionId, SessionInfo};
use serde_json::Value;
use tokio::sync::Notify;

use crate::turn;

mod actor;
mod traits;

pub(crate) use actor::{ActorHandle, CancelToken};
pub use traits::{
    EventSink, LlmClientAdapter, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler,
    RuntimeConfig, RuntimeError, ToolProvider,
};

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
    #[allow(dead_code)]
    pub(crate) fn take_background_request(&self, session: &str) -> bool {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.is_some_and(|handle| handle.background.swap(false, Ordering::SeqCst))
    }

    /// Check whether a background detach has been requested for `session`
    /// without clearing it, so parallel foreground waits in the same step can
    /// all observe the request.
    pub(crate) fn has_background_request(&self, session: &str) -> bool {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.is_some_and(|handle| handle.background.load(Ordering::SeqCst))
    }

    /// Clear any pending background request for `session`.
    pub(crate) fn clear_background_request(&self, session: &str) {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        if let Some(handle) = handle {
            handle.background.store(false, Ordering::SeqCst);
        }
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

    /// Enqueue an inbox item in SQLite and publish its durable
    /// `session.inbox.enqueued` event so live SSE clients see the message
    /// without refreshing.
    pub(crate) async fn enqueue_and_emit(
        &self,
        inbox: oc_store::NewInbox,
    ) -> Result<Value, RuntimeError> {
        let session_id = inbox.session_id.clone();
        let inbox_id = inbox.id.clone();
        let item = self.inner.config.store.enqueue(inbox).await?;
        let directory = self
            .inner
            .config
            .store
            .get_session(&session_id)
            .await?
            .and_then(|value| {
                value
                    .pointer("/location/directory")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        if let Ok(parsed) = serde_json::from_value::<oc_types::InboxItem>(item.clone()) {
            self.emit(
                &session_id,
                &directory,
                oc_types::payload::SessionInboxEnqueued {
                    session_id: SessionId::from_wire(session_id.clone()),
                    inbox_id: oc_types::MessageId::from_wire(inbox_id),
                    item: parsed,
                },
            )
            .await?;
        }
        Ok(item)
    }

    pub(crate) fn session_id(session: &str) -> SessionId {
        SessionId::from_wire(session.to_owned())
    }
}

impl Runtime {
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
    pub(super) async fn run_execution(
        &self,
        session: &str,
        item: &Value,
        cancel: &Arc<CancelToken>,
        handle: &Arc<ActorHandle>,
    ) -> Result<(), RuntimeError> {
        turn::run_execution(self, session, item, cancel, handle).await
    }
}
