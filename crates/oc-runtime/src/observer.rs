//! Runtime wiring for `oc-observer`.
//!
//! The runtime feeds flight strips to an [`ObserverEngine`] and hosts the two
//! long-lived tasks the engine needs:
//!
//! - one ticker per root session, started on the first [`Runtime::notify`],
//!   which builds the tree/goal inputs and applies non-repair verdicts;
//! - one repair worker, started by [`Runtime::start`], which drains
//!   [`RepairCommand`]s produced by the host [`RepairExecutor`].
//!
//! The observer never sees tool arguments or output. Only [`FlightStrip`]
//! fields cross the boundary.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oc_observer::{
    BoxFuture, FlightStrip, GoalSnapshot, GoalStore, HostFlag, ObserverAction, ObserverEngine,
    ObserverError, ObserverRun, RepairExecutor, RepairKind, RepairOutcome, RootContext, StripState,
    TreeRow,
};
use oc_store::{Delivery, MessageQuery, NewInbox, Order, SessionPatch};
use oc_types::payload::SessionModelSelected;
use oc_types::{MessageId, SessionId, now_millis};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::runtime::{Runtime, RuntimeError};

/// Upper bound on tree rows fed to one observer tick.
const MAX_TREE_ROWS: usize = 100;
/// Floor for the ticker interval so a zero config cannot spin.
const MIN_TICK_MS: u64 = 10;

/// A repair command the observer asks the runtime to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairCommand {
    /// Cancel the session's current execution.
    Interrupt {
        /// Target session.
        session_id: String,
    },
    /// Cancel one tool call. The runtime cancels whole executions, so this is
    /// a typed no-op until per-call cancellation exists.
    CancelTool {
        /// Provider tool-call id.
        tool_call_id: String,
    },
    /// Restart a child session.
    RestartSubagent {
        /// Child session.
        session_id: String,
        /// Observer note.
        note: String,
    },
    /// Force compaction on a session.
    Compact {
        /// Target session.
        session_id: String,
    },
    /// Switch a session to another model.
    SwitchModel {
        /// Target session.
        session_id: String,
        /// `provider/id` or `provider/id#variant`.
        model: String,
    },
    /// Re-deliver text to a session.
    Repost {
        /// Target session.
        session_id: String,
        /// Text to inject.
        text: String,
    },
}

/// A connected observer repair pipe.
///
/// The engine receives the [`RepairExecutor`] half; the runtime owns the
/// receiver and applies commands from its repair worker.
#[derive(Debug)]
pub struct RepairChannel;

impl RepairChannel {
    /// Create the executor half and the command receiver.
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (Arc<dyn RepairExecutor>, UnboundedReceiver<RepairCommand>) {
        let (sender, receiver) = unbounded_channel();
        (Arc::new(ChannelExecutor { sender }), receiver)
    }
}

struct ChannelExecutor {
    sender: UnboundedSender<RepairCommand>,
}

impl RepairExecutor for ChannelExecutor {
    fn apply<'a>(
        &'a self,
        verdict: &'a oc_observer::ObserverVerdict,
    ) -> BoxFuture<'a, Result<RepairOutcome, ObserverError>> {
        Box::pin(async move {
            let command = command_from_verdict(verdict)?;
            self.sender
                .send(command)
                .map_err(|_| ObserverError::Executor("repair worker is stopped".to_owned()))?;
            Ok(RepairOutcome {
                applied: true,
                detail: None,
            })
        })
    }
}

/// Translate a validated repair verdict into a runtime command.
///
/// `retry_tool` has no runtime command yet, so it is reported as an explicit
/// executor error (the engine then publishes `observer.repair.failed`). An
/// absent `switch_model` model or `repost` text is left empty; the runtime
/// resolves those defaults when it applies the command.
fn command_from_verdict(
    verdict: &oc_observer::ObserverVerdict,
) -> Result<RepairCommand, ObserverError> {
    let target = verdict.target.clone().unwrap_or_default();
    if target.is_empty() {
        return Err(ObserverError::Executor(
            "repair verdict has no target".to_owned(),
        ));
    }
    let repair = verdict
        .repair
        .as_ref()
        .ok_or_else(|| ObserverError::Executor("repair verdict has no repair".to_owned()))?;
    let kind = repair
        .kind
        .ok_or_else(|| ObserverError::Executor("repair verdict has no kind".to_owned()))?;
    let args = repair.args.as_object();
    match kind {
        RepairKind::RetryTool => Err(ObserverError::Executor(
            "retry_tool is not wired into the runtime yet".to_owned(),
        )),
        RepairKind::CancelTool => Ok(RepairCommand::CancelTool {
            tool_call_id: target,
        }),
        RepairKind::RestartSubagent => Ok(RepairCommand::RestartSubagent {
            session_id: target,
            note: args
                .and_then(|args| args.get("note"))
                .and_then(Value::as_str)
                .map_or_else(|| verdict.reason.clone(), str::to_owned),
        }),
        RepairKind::CompactContext => Ok(RepairCommand::Compact { session_id: target }),
        RepairKind::SwitchModel => Ok(RepairCommand::SwitchModel {
            session_id: target,
            // An absent model means "use observer.escalation_model"; the
            // runtime resolves it when the command is applied.
            model: args
                .and_then(|args| args.get("model"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        RepairKind::Repost => Ok(RepairCommand::Repost {
            session_id: target,
            // An absent text means "re-deliver the last user prompt"; the
            // runtime loads it from the transcript when the command is applied.
            text: args
                .and_then(|args| args.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
    }
}

/// Resolve the model a `switch_model` repair should apply.
///
/// An explicit model always wins. An empty model falls back to
/// `observer.escalation_model`; without one the repair is a config error and
/// nothing is switched.
fn switch_model_target(
    requested: String,
    configured: Option<String>,
    session: &str,
) -> Result<String, RuntimeError> {
    if !requested.is_empty() {
        return Ok(requested);
    }
    configured.ok_or_else(|| {
        RuntimeError::Config(format!(
            "switch_model repair for {session} has no model and \
             observer.escalation_model is not configured"
        ))
    })
}

/// Observer collaborators attached to one [`Runtime`].
pub struct ObserverBinding {
    /// The observer loop.
    pub engine: Arc<ObserverEngine>,
    /// Goal persistence used by each tick.
    pub goals: Arc<dyn GoalStore>,
    /// Repairs produced by the engine's executor.
    pub repairs: UnboundedReceiver<RepairCommand>,
}

impl std::fmt::Debug for ObserverBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObserverBinding")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// Shared observer state owned by [`crate::runtime::Inner`].
pub(crate) struct ObserverHost {
    pub(crate) engine: Arc<ObserverEngine>,
    goals: Arc<dyn GoalStore>,
    repairs: Mutex<Option<UnboundedReceiver<RepairCommand>>>,
    tickers: Mutex<HashMap<String, JoinHandle<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for ObserverHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObserverHost")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl ObserverHost {
    pub(crate) fn new(binding: ObserverBinding) -> Self {
        Self {
            engine: binding.engine,
            goals: binding.goals,
            repairs: Mutex::new(Some(binding.repairs)),
            tickers: Mutex::new(HashMap::new()),
            worker: Mutex::new(None),
        }
    }

    /// Start the repair worker exactly once.
    pub(crate) fn spawn_worker(self: &Arc<Self>, runtime: &Runtime) {
        let mut slot = self.worker.lock().expect("observer worker slot");
        if slot.is_some() {
            return;
        }
        let Some(receiver) = self.repairs.lock().expect("observer repairs slot").take() else {
            return;
        };
        let runtime = runtime.clone();
        *slot = Some(tokio::spawn(async move {
            runtime.observer_repair_loop(receiver).await;
        }));
    }

    fn drain_tickers(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut tickers = self.tickers.lock().expect("observer tickers");
            tickers.drain().map(|(_, handle)| handle).collect()
        };
        for handle in handles {
            handle.abort();
        }
        if let Some(worker) = self.worker.lock().expect("observer worker slot").take() {
            worker.abort();
        }
    }
}

impl Runtime {
    /// Attach an observer engine, goal store and repair receiver.
    ///
    /// The binding is optional, so existing hosts keep working unchanged.
    /// [`Runtime::start`] spawns the repair worker; [`Runtime::notify`] spawns
    /// one ticker per root session.
    #[must_use]
    pub fn with_observer(self, binding: ObserverBinding) -> Self {
        let host = Arc::new(ObserverHost::new(binding));
        *self.inner.observer.lock().expect("observer slot") = Some(Arc::clone(&host));
        if self.inner.started.load(std::sync::atomic::Ordering::SeqCst) {
            host.spawn_worker(&self);
        }
        self
    }

    /// The attached observer engine, when configured.
    #[must_use]
    pub fn observer(&self) -> Option<Arc<ObserverEngine>> {
        self.observer_host().map(|host| Arc::clone(&host.engine))
    }

    pub(crate) fn observer_host(&self) -> Option<Arc<ObserverHost>> {
        self.inner.observer.lock().expect("observer slot").clone()
    }

    pub(crate) fn start_observer_worker(&self) {
        if let Some(host) = self.observer_host() {
            host.spawn_worker(self);
        }
    }

    pub(crate) fn stop_observer(&self) {
        if let Some(host) = self.observer_host() {
            host.drain_tickers();
        }
    }

    /// Start one ticker for the root that owns `session`, if not running.
    pub(crate) async fn ensure_observer_ticker(&self, session: &str) {
        let Some(host) = self.observer_host() else {
            return;
        };
        let root = match self.inner.config.store.session_node(session).await {
            Ok(Some(node)) => node.root_id,
            Ok(None) => session.to_owned(),
            Err(error) => {
                tracing::warn!(session = %session, error = %error, "observer ticker: session node lookup failed");
                session.to_owned()
            }
        };
        let mut tickers = host.tickers.lock().expect("observer tickers");
        if tickers.contains_key(&root) {
            return;
        }
        let runtime = self.clone();
        let task_host = Arc::clone(&host);
        let task_root = root.clone();
        let join = tokio::spawn(async move {
            runtime.observer_ticker(task_host, task_root).await;
        });
        tickers.insert(root, join);
    }

    async fn observer_ticker(&self, host: Arc<ObserverHost>, root: String) {
        let interval = Duration::from_millis(host.engine.config().tick_ms.max(MIN_TICK_MS));
        loop {
            if self.inner.shutdown.is_cancelled() {
                return;
            }
            self.observer_pass(&host, &root).await;
            tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// Force one observer check for `root_id` (UI `POST /observer/check` and
    /// the `goal_status`/`goal_complete` tools).
    ///
    /// Builds the same tree and goal inputs as the ticker, calls
    /// [`ObserverEngine::check_now`] and applies the verdict through the normal
    /// run path. Missing observer configuration is a [`RuntimeError::Config`];
    /// store/engine failures are [`RuntimeError::Task`].
    pub async fn request_goal_check(&self, root_id: &str) -> Result<(), RuntimeError> {
        let Some(host) = self.observer_host() else {
            return Err(RuntimeError::Config(
                "observer is not configured; cannot run a goal check".to_owned(),
            ));
        };
        let tree = build_tree(&self.inner.config.store, root_id)
            .await
            .map_err(|error| {
                RuntimeError::Task(format!("goal check tree lookup failed: {error}"))
            })?;
        let goal = host
            .goals
            .load(root_id)
            .await
            .map_err(|error| RuntimeError::Task(format!("goal check load failed: {error}")))?;
        let run = host.engine.check_now(root_id, tree, goal).await;
        self.apply_observer_run(root_id, run).await;
        Ok(())
    }

    async fn observer_pass(&self, host: &ObserverHost, root: &str) {
        let node = match self.inner.config.store.session_node(root).await {
            Ok(node) => node,
            Err(error) => {
                tracing::warn!(root = %root, error = %error, "observer tick: session node lookup failed");
                return;
            }
        };
        let Some(node) = node else {
            return;
        };
        host.engine.set_root_context(
            root,
            RootContext {
                agent: Some(node.agent.clone()),
                model: node.model.clone(),
                step: 0,
                state: Some(node.status.clone()),
            },
        );
        let tree = match build_tree(&self.inner.config.store, root).await {
            Ok(tree) => tree,
            Err(error) => {
                tracing::warn!(root = %root, error = %error, "observer tick: tree lookup failed");
                return;
            }
        };
        let goal: Option<GoalSnapshot> = match host.goals.load(root).await {
            Ok(goal) => goal,
            Err(error) => {
                tracing::warn!(root = %root, error = %error, "observer tick: goal load failed");
                None
            }
        };
        let run = host.engine.tick(root, tree, goal).await;
        self.apply_observer_run(root, run).await;
    }

    async fn apply_observer_run(&self, root: &str, run: Option<ObserverRun>) {
        match run {
            Some(ObserverRun::Verdict { verdict }) => match verdict.action {
                ObserverAction::Nudge => {
                    let target = verdict.target.unwrap_or_default();
                    let text = verdict.message.unwrap_or_default();
                    if target.is_empty() || text.is_empty() {
                        tracing::warn!(root = %root, "observer nudge without target or message");
                        return;
                    }
                    if let Err(error) = self.observer_synthetic(&target, &text, root).await {
                        tracing::warn!(root = %root, target = %target, error = %error, "observer nudge enqueue failed");
                        return;
                    }
                    if target != root
                        && let Err(error) = self.notify_local(root).await
                    {
                        tracing::warn!(root = %root, error = %error, "observer nudge root notify failed");
                    }
                }
                ObserverAction::Inspect => {
                    // Gap: `digest` is not in the runtime yet. A digest result
                    // would be the only legitimate way to feed the observer
                    // more context; inventing one here would fake data.
                    tracing::warn!(
                        root = %root,
                        target = %verdict.target.unwrap_or_default(),
                        "observer inspect requested but digest is not wired; no digest produced"
                    );
                }
                _ => {}
            },
            Some(ObserverRun::Repair { verdict, outcome }) => {
                tracing::debug!(
                    root = %root,
                    target = %verdict.target.unwrap_or_default(),
                    applied = outcome.applied,
                    "observer repair dispatched"
                );
            }
            Some(ObserverRun::Rejected { action, reason }) => {
                tracing::warn!(root = %root, action = action.as_str(), reason = %reason, "observer verdict rejected");
            }
            Some(ObserverRun::Escalated { reason, .. }) => {
                tracing::warn!(root = %root, reason = %reason, "observer repair budget exhausted");
            }
            Some(ObserverRun::Unavailable { reason }) => {
                tracing::warn!(root = %root, reason = %reason, "observer unavailable");
            }
            Some(ObserverRun::Skipped { reason }) => {
                tracing::trace!(root = %root, reason = ?reason, "observer tick skipped");
            }
            None => {}
        }
    }

    /// Record execution-level host flags so the next tick sees them.
    pub(crate) async fn note_observer_execution(&self, session: &str, failed: bool) {
        let Some(engine) = self.observer() else {
            return;
        };
        let root = match self.inner.config.store.session_node(session).await {
            Ok(Some(node)) => node.root_id,
            Ok(None) => session.to_owned(),
            Err(error) => {
                tracing::warn!(session = %session, error = %error, "observer host event: node lookup failed");
                session.to_owned()
            }
        };
        let flag = if failed {
            HostFlag::ExecutionFailed
        } else {
            HostFlag::ExecutionInterrupted
        };
        engine.note_host_event(&root, flag);
    }

    /// Enqueue an observer synthetic message and notify its session.
    pub(crate) async fn observer_synthetic(
        &self,
        session: &str,
        text: &str,
        root: &str,
    ) -> Result<(), RuntimeError> {
        let payload = json!({
            "text": text,
            "metadata": { "observer": true, "target": root },
            "time": { "created": now_millis() },
        });
        self.inner
            .config
            .store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: session.to_owned(),
                kind: "synthetic".to_owned(),
                payload,
                delivery: Delivery::Steer,
            })
            .await?;
        self.notify_local(session).await
    }

    async fn observer_compaction(
        &self,
        session: &str,
        note: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let mut payload = serde_json::Map::new();
        payload.insert("time".to_owned(), json!({ "created": now_millis() }));
        payload.insert("status".to_owned(), Value::String("pending".to_owned()));
        payload.insert("reason".to_owned(), Value::String("observer".to_owned()));
        if let Some(note) = note {
            payload.insert("note".to_owned(), Value::String(note.to_owned()));
        }
        self.inner
            .config
            .store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: session.to_owned(),
                kind: "compaction".to_owned(),
                payload: Value::Object(payload),
                delivery: Delivery::Queue,
            })
            .await?;
        self.notify_local(session).await
    }

    async fn observer_switch_model(&self, session: &str, model: &str) -> Result<(), RuntimeError> {
        let value = self
            .inner
            .config
            .store
            .get_session(session)
            .await?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
        let info = Runtime::parse_session(&value)?;
        let model_ref = crate::prompt::model_ref(&Value::String(model.to_owned()))
            .ok_or_else(|| RuntimeError::Config(format!("invalid model reference {model:?}")))?;
        // A repair must never persist a model the catalog cannot run: every
        // later turn would fail at `resolve_turn` and the session would be
        // stuck until a human patches it.
        if let Err(error) = self.inner.config.catalog.resolve(model) {
            return Err(RuntimeError::Config(format!(
                "repair model {model:?} is not in the catalog: {error}"
            )));
        }
        let previous = info.model.clone();
        self.inner
            .config
            .store
            .patch_session(
                session,
                SessionPatch {
                    model: Some(
                        serde_json::to_value(&model_ref)
                            .map_err(|error| RuntimeError::Config(error.to_string()))?,
                    ),
                    ..SessionPatch::default()
                },
            )
            .await?;
        self.emit(
            session,
            &info.location.directory,
            SessionModelSelected {
                session_id: SessionId::from_wire(session.to_owned()),
                model: model_ref,
                previous,
            },
        )
        .await
    }

    async fn observer_repair_loop(&self, mut receiver: UnboundedReceiver<RepairCommand>) {
        loop {
            let command = tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => return,
                command = receiver.recv() => command,
            };
            let Some(command) = command else {
                return;
            };
            if let Err(error) = self.apply_repair_command(command).await {
                tracing::warn!(error = %error, "observer repair failed");
            }
        }
    }

    async fn apply_repair_command(&self, command: RepairCommand) -> Result<(), RuntimeError> {
        match command {
            RepairCommand::Interrupt { session_id } => {
                if !self.interrupt(&session_id).await? {
                    tracing::debug!(session = %session_id, "observer interrupt found no running execution");
                }
            }
            RepairCommand::CancelTool { tool_call_id } => {
                tracing::warn!(
                    tool_call_id = %tool_call_id,
                    "cancel_tool repair is not supported: the runtime has no per-tool token; use interrupt"
                );
            }
            RepairCommand::RestartSubagent { session_id, note } => {
                if !self.interrupt(&session_id).await? {
                    tracing::debug!(session = %session_id, "observer restart found no running execution");
                }
                let Some(prompt) = self.session_text(&session_id, "user", Order::Asc).await? else {
                    tracing::warn!(
                        session = %session_id,
                        "restart_subagent skipped: the child has no user prompt to replay"
                    );
                    return Ok(());
                };
                let text = format!("{prompt}\n\n[observer restart] {note}");
                let root = self.session_root(&session_id).await?;
                self.observer_synthetic(&session_id, &text, &root).await?;
            }
            RepairCommand::Compact { session_id } => {
                self.observer_compaction(&session_id, None).await?;
            }
            RepairCommand::SwitchModel { session_id, model } => {
                let model =
                    switch_model_target(model, self.configured_escalation_model(), &session_id)?;
                self.observer_switch_model(&session_id, &model).await?;
            }
            RepairCommand::Repost { session_id, text } => {
                let text = if text.is_empty() {
                    let Some(text) = self.session_text(&session_id, "user", Order::Desc).await?
                    else {
                        tracing::warn!(
                            session = %session_id,
                            "repost skipped: the session has no user prompt to re-deliver"
                        );
                        return Ok(());
                    };
                    text
                } else {
                    text
                };
                if self.has_pending_deliverable(&session_id).await? {
                    tracing::info!(
                        session = %session_id,
                        "repost skipped: the session already has pending input"
                    );
                    return Ok(());
                }
                // The running prompt has already left the inbox, so the guard
                // above cannot see it. Re-injecting it mid-turn would execute
                // the same instructions twice.
                if !self.is_idle(&session_id).await? {
                    tracing::info!(
                        session = %session_id,
                        "repost skipped: a turn is already running"
                    );
                    return Ok(());
                }
                let root = self.session_root(&session_id).await?;
                self.observer_synthetic(&session_id, &text, &root).await?;
            }
        }
        Ok(())
    }

    /// Text of the first/last message of `kind` in a session's transcript.
    async fn session_text(
        &self,
        session: &str,
        kind: &str,
        order: Order,
    ) -> Result<Option<String>, RuntimeError> {
        let page = self
            .inner
            .config
            .store
            .list_messages(MessageQuery {
                session_id: session.to_owned(),
                limit: 1,
                order,
                cursor: None,
                kind: Some(kind.to_owned()),
            })
            .await?;
        Ok(page
            .items
            .first()
            .and_then(|message| message.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    /// Root session that owns `session` (the session itself when unknown).
    async fn session_root(&self, session: &str) -> Result<String, RuntimeError> {
        Ok(match self.inner.config.store.session_node(session).await? {
            Some(node) => node.root_id,
            None => session.to_owned(),
        })
    }

    /// Whether the session has input the runner would deliver.
    async fn has_pending_deliverable(&self, session: &str) -> Result<bool, RuntimeError> {
        let items = self.inner.config.store.list_inbox(session).await?;
        Ok(items.iter().any(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("user" | "synthetic" | "peer" | "observer")
            )
        }))
    }

    /// `observer.escalation_model` from the merged config, when set.
    fn configured_escalation_model(&self) -> Option<String> {
        self.inner
            .config
            .config
            .merged()
            .pointer("/observer/escalation_model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned)
    }
}

/// Breadth-first tree rows, root first, capped at [`MAX_TREE_ROWS`].
async fn build_tree(
    store: &oc_store::Store,
    root: &str,
) -> Result<Vec<TreeRow>, oc_store::StoreError> {
    let mut rows = Vec::new();
    let mut queue = VecDeque::new();
    if let Some(node) = store.session_node(root).await? {
        rows.push(TreeRow {
            sid: node.id.clone(),
            parent: None,
            agent: node.agent,
            state: node.status,
            depth: node.depth,
        });
        queue.push_back(node.id);
    }
    while let Some(parent) = queue.pop_front() {
        if rows.len() >= MAX_TREE_ROWS {
            break;
        }
        for child in store.children(&parent).await? {
            if rows.len() >= MAX_TREE_ROWS {
                break;
            }
            let id = child.id.clone();
            rows.push(TreeRow {
                sid: id.clone(),
                parent: child.parent_id,
                agent: child.agent,
                state: child.status,
                depth: child.depth,
            });
            queue.push_back(id);
        }
    }
    Ok(rows)
}

/// Map one tool outcome to the settle state and one-line outcome.
pub(crate) fn strip_result(outcome: &crate::turn::ToolOutcome) -> (StripState, Option<String>) {
    match outcome {
        crate::turn::ToolOutcome::Completed { .. } => (StripState::Ok, Some("ok".to_owned())),
        crate::turn::ToolOutcome::Failed { error, .. } => {
            (StripState::Error, Some(error.message.clone()))
        }
        crate::turn::ToolOutcome::Cancelled => {
            (StripState::Cancelled, Some("cancelled".to_owned()))
        }
    }
}

/// Build the flight strip for one dispatched tool call.
pub(crate) fn flight_strip(
    root_id: &str,
    session_id: &str,
    agent: &str,
    depth: u32,
    call: &crate::turn::PendingToolCall,
) -> Option<FlightStrip> {
    let expectation = call.expectation.as_ref()?;
    Some(FlightStrip {
        id: oc_types::ObservationId::new().into_inner(),
        root_id: root_id.to_owned(),
        session_id: session_id.to_owned(),
        agent: agent.to_owned(),
        tool_call_id: call.id.clone(),
        tool: call.name.clone(),
        why: expectation.why.clone(),
        expect_s: expectation.expect_seconds,
        intervene_s: expectation.intervene_after_seconds,
        started_ms: now_millis(),
        settled_ms: None,
        state: StripState::Running,
        outcome: None,
        parent_call: None,
        depth,
        retryable: retryable_tool(&call.name),
        retry_of: None,
    })
}

/// Tools the observer may re-run without side effects.
fn retryable_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read" | "glob" | "grep" | "lsp" | "digest" | "roster" | "memory_search"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(kind: RepairKind, args: Value) -> oc_observer::ObserverVerdict {
        oc_observer::ObserverVerdict {
            action: ObserverAction::Repair,
            target: Some("ses_target".to_owned()),
            reason: "the target is stuck".to_owned(),
            message: Some("verdict message".to_owned()),
            repair: Some(oc_observer::Repair {
                kind: Some(kind),
                args,
            }),
            goal_evidence: Vec::new(),
        }
    }

    #[test]
    fn switch_model_without_args_maps_to_an_empty_model() {
        let command = command_from_verdict(&verdict(RepairKind::SwitchModel, json!({})))
            .expect("missing model is resolved by the runtime");
        assert_eq!(
            command,
            RepairCommand::SwitchModel {
                session_id: "ses_target".to_owned(),
                model: String::new(),
            }
        );
    }

    #[test]
    fn switch_model_keeps_an_explicit_model() {
        let command = command_from_verdict(&verdict(
            RepairKind::SwitchModel,
            json!({"model": "test/beta"}),
        ))
        .expect("explicit model");
        assert_eq!(
            command,
            RepairCommand::SwitchModel {
                session_id: "ses_target".to_owned(),
                model: "test/beta".to_owned(),
            }
        );
    }

    #[test]
    fn repost_without_text_maps_to_empty_text() {
        let command = command_from_verdict(&verdict(RepairKind::Repost, json!({})))
            .expect("missing text is resolved from the transcript");
        assert_eq!(
            command,
            RepairCommand::Repost {
                session_id: "ses_target".to_owned(),
                text: String::new(),
            }
        );
    }

    #[test]
    fn repost_keeps_an_explicit_text() {
        let command =
            command_from_verdict(&verdict(RepairKind::Repost, json!({"text": "try again"})))
                .expect("explicit text");
        assert_eq!(
            command,
            RepairCommand::Repost {
                session_id: "ses_target".to_owned(),
                text: "try again".to_owned(),
            }
        );
    }

    #[test]
    fn switch_model_prefers_the_explicit_model_over_the_fallback() {
        let model = switch_model_target(
            "test/beta".to_owned(),
            Some("test/escalation".to_owned()),
            "ses_target",
        )
        .expect("explicit model wins");
        assert_eq!(model, "test/beta");
    }

    #[test]
    fn empty_switch_model_uses_the_escalation_model() {
        let model = switch_model_target(
            String::new(),
            Some("test/escalation".to_owned()),
            "ses_target",
        )
        .expect("configured escalation model");
        assert_eq!(model, "test/escalation");
    }

    #[test]
    fn empty_switch_model_without_an_escalation_model_is_a_config_error() {
        let error = switch_model_target(String::new(), None, "ses_target")
            .expect_err("no configured escalation model");
        assert!(matches!(error, RuntimeError::Config(_)));
        assert!(error.to_string().contains("observer.escalation_model"));
    }
}
