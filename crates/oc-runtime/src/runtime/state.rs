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
