use std::sync::Arc;
use std::time::Duration;

use oc_observer::{GoalSnapshot, HostFlag, ObserverAction, ObserverRun, RootContext};

use crate::runtime::{Runtime, RuntimeError};

use super::MIN_TICK_MS;
use super::host::ObserverHost;
use super::strips::build_tree;

impl Runtime {
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
    /// [`oc_observer::ObserverEngine::check_now`] and applies the verdict through the normal
    /// run path. Missing observer configuration is a [`RuntimeError::Config`];
    /// store/engine failures are [`RuntimeError::Task`].
    ///
    /// When the root has a goal the goal judge runs too, in the goal's own
    /// strict verdict format (`check_goal`). That is the decision `goal_complete`
    /// waits for: its transition is published as `ObserverNotice::Goal`, which
    /// the host persists as `goal.updated`. A terminal goal is skipped, so a
    /// goal cannot be closed twice by two overlapping checks.
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
        if let Some(goal) = goal.as_ref()
            && !goal.status.is_terminal()
        {
            let recent = host.engine.recent_outcomes(root_id, 12);
            let known = self.recent_message_ids(root_id).await;
            if let Some(transition) = host.engine.check_goal(root_id, goal, &recent, &known).await {
                // Persist synchronously: the notifier's own write is spawned,
                // so without this the caller could read the goal row before
                // the decided status is committed. Same values, so the later
                // notifier write is a no-op in status/rounds/stalls/evidence.
                host.goals.save(&transition.goal).await.map_err(|error| {
                    RuntimeError::Task(format!("goal check save failed: {error}"))
                })?;
                self.record_evaluation(root_id, "goal", transition.applied, &transition.reason)
                    .await;
            }
        }
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
        // Persist exactly one `observation` row for the evaluation this run
        // represents; a coalesced `Skipped` tick performed no evaluation and
        // records nothing. This is the only write for an observer run, so a
        // goal check records its row separately (see `request_goal_check`).
        self.record_observer_evaluation(root, run.as_ref()).await;
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

    /// Persist one `observation` row for one model-backed evaluation.
    ///
    /// `observation` is documented as the flight-strip ledger, but before this
    /// write it stayed empty no matter how many ticks ran: the registry is
    /// memory-only and nothing ever mirrored a strip to a row. One row is
    /// written per evaluation, never per event; a coalesced tick performed no
    /// evaluation and is not recorded.
    ///
    /// The row is honest about what it is: `tool = "observer"`, `synthetic`
    /// (no agent declared it), no tool call, and zero expectation clocks, since
    /// an evaluation has no declared duration. It is settled immediately so the
    /// ledger does not fill with rows stuck in `running`.
    async fn record_evaluation(&self, root: &str, kind: &str, ok: bool, reason: &str) {
        let id = oc_types::ObservationId::new().into_inner();
        let now = oc_types::now_millis();
        let store = &self.inner.config.store;
        let new = oc_store::NewObservation {
            id: id.clone(),
            root_id: root.to_owned(),
            session_id: root.to_owned(),
            tool_call_id: String::new(),
            tool: "observer".to_owned(),
            why: format!("observer evaluation: {kind}"),
            expect_s: 0,
            intervene_s: 0,
            started: now,
            parent_call: None,
            synthetic: true,
        };
        if let Err(error) = store.record_observation(new).await {
            tracing::warn!(root = %root, %error, "observer evaluation row write failed");
            return;
        }
        let state = if ok { "ok" } else { "error" };
        if let Err(error) = store
            .settle_observation(&id, state, Some(reason), now)
            .await
        {
            tracing::warn!(root = %root, %error, "observer evaluation row settle failed");
        }
    }

    /// Record the evaluation a [`ObserverRun`] represents, if any.
    async fn record_observer_evaluation(&self, root: &str, run: Option<&ObserverRun>) {
        let Some(run) = run else {
            return;
        };
        let (kind, reason, ok) = match run {
            // Coalescing means no evaluation happened; nothing to record.
            ObserverRun::Skipped { .. } => return,
            ObserverRun::Verdict { verdict } => {
                (verdict.action.as_str(), verdict.reason.as_str(), true)
            }
            ObserverRun::Repair { verdict, outcome } => {
                ("repair", verdict.reason.as_str(), outcome.applied)
            }
            ObserverRun::Rejected { action, reason } => (action.as_str(), reason.as_str(), false),
            ObserverRun::Escalated { reason, .. } => ("escalated", reason.as_str(), false),
            ObserverRun::Unavailable { reason } => ("unavailable", reason.as_str(), false),
        };
        self.record_evaluation(root, kind, ok, reason).await;
    }

    /// Recent message ids of a root, as goal-evidence candidates.
    ///
    /// `goal_complete` evidence is usually a message id; without this the judge
    /// would reject every `done` verdict because [`oc_observer::GoalSnapshot`]
    /// only knows tool-call ids.
    async fn recent_message_ids(&self, root: &str) -> Vec<String> {
        let query = oc_store::MessageQuery {
            session_id: root.to_owned(),
            limit: 50,
            order: oc_store::Order::Desc,
            cursor: None,
            kind: None,
        };
        match self.inner.config.store.list_messages(query).await {
            Ok(page) => page
                .items
                .iter()
                .filter_map(|item| item.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_owned)
                .collect(),
            Err(error) => {
                tracing::warn!(root = %root, %error, "goal check: message id lookup failed");
                Vec::new()
            }
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
}
