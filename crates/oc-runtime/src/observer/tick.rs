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
}
