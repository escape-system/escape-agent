use std::sync::Arc;
use std::sync::atomic::Ordering;

use oc_types::SessionInfo;
use serde_json::Value;

use crate::turn;

use super::{ActorHandle, CancelToken, Runtime, RuntimeError};

impl Runtime {
    /// Mirror the in-flight step number into the session's actor.
    ///
    /// The counter is best-effort observability: a session without an actor
    /// (idle, or never started) has no step and the call is a no-op.
    pub(crate) fn set_step(&self, session: &str, step: u32) {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        if let Some(handle) = handle {
            handle.step.store(step, Ordering::SeqCst);
        }
    }

    /// The in-flight step number for `session`, or `None` when the session has
    /// no actor. An idle/unstarted session yields `None`, which the observer
    /// renders as step `0`.
    pub(crate) fn current_step(&self, session: &str) -> Option<u32> {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.map(|handle| handle.step.load(Ordering::SeqCst))
    }

    /// Publish the live step cap for the observer. `0` means "unknown" (idle
    /// or never-started session) and is omitted on the observer input wire.
    pub(crate) fn set_step_budget(&self, session: &str, cap: u32) {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        if let Some(handle) = handle {
            handle.step_cap.store(cap, Ordering::SeqCst);
        }
    }

    /// Grant extra steps to an active turn (consumed at the next step boundary).
    pub(crate) fn grant_step_budget(&self, session: &str, steps: u32) -> bool {
        if steps == 0 {
            return false;
        }
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        let Some(handle) = handle else {
            return false;
        };
        if !handle.running.load(Ordering::SeqCst) {
            return false;
        }
        handle.extra_steps.fetch_add(steps, Ordering::SeqCst);
        true
    }

    /// Drain any step grant accumulated while the previous step ran.
    pub(crate) fn take_granted_steps(&self, session: &str) -> u32 {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.map_or(0, |handle| handle.extra_steps.swap(0, Ordering::SeqCst))
    }

    /// Reset all live step counters and pending observer grants at turn exit.
    pub(crate) fn reset_step_budget(&self, session: &str) {
        self.set_step(session, 0);
        self.set_step_budget(session, 0);
        let _ = self.take_granted_steps(session);
    }

    pub(crate) fn current_step_cap(&self, session: &str) -> Option<u32> {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.and_then(|handle| {
            let cap = handle.step_cap.load(Ordering::SeqCst);
            (cap > 0).then_some(cap)
        })
    }

    pub(crate) fn current_steps_remaining(&self, session: &str) -> Option<u32> {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session).cloned()
        };
        handle.and_then(|handle| {
            let cap = handle.step_cap.load(Ordering::SeqCst);
            let step = handle.step.load(Ordering::SeqCst);
            if cap == 0 {
                return None;
            }
            Some(cap.saturating_sub(step))
        })
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

    /// Whether a deliverable inbox row may be promoted as the next execution.
    ///
    /// Both `steer` and `queue` start a turn when a session is idle (and run FIFO
    /// when a running turn ends). Mid-turn injection (`turn::run::promote_steers`)
    /// promotes only `steer`, keeping `queue` rows queued until the current turn
    /// completes.
    pub(crate) fn delivery_admittable(item: &Value) -> bool {
        let delivery = item.get("delivery").and_then(Value::as_str);
        matches!(delivery, Some("steer" | "queue") | None)
    }

    /// First deliverable inbox row, in enqueue order. `compaction` rows are
    /// delivered to the actor, which runs them itself; `move` is still skipped
    /// because no handler exists. Both `steer` and `queue` rows are eligible to
    /// wake an idle actor so queued prompts/notes are never stranded.
    pub(crate) async fn next_deliverable(
        &self,
        session: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let items = self.inner.config.store.list_inbox(session).await?;
        Ok(items.into_iter().find(|item| {
            let deliverable = matches!(
                item.get("type").and_then(Value::as_str),
                Some("user" | "synthetic" | "peer" | "observer" | "compaction")
            );
            deliverable && Self::delivery_admittable(item)
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
    pub(crate) async fn run_execution(
        &self,
        session: &str,
        item: &Value,
        cancel: &Arc<CancelToken>,
        handle: &Arc<ActorHandle>,
    ) -> Result<(), RuntimeError> {
        Box::pin(turn::run_execution(self, session, item, cancel, handle)).await
    }
}
