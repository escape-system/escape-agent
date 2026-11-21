use oc_types::InterruptReason;
use serde_json::Value;

use super::{Runtime, RuntimeError};

impl Runtime {
    /// Cancel the session's current execution and all its descendants. Returns `false` when idle.
    #[allow(clippy::unused_async_trait_impl)]
    pub async fn interrupt(&self, session_id: &str) -> Result<bool, RuntimeError> {
        self.interrupt_with_reason(session_id, InterruptReason::User)
            .await
    }

    /// Cancel the session's execution and all its descendants, recording
    /// `reason` on every cancelled execution.
    ///
    /// User stops pass [`InterruptReason::User`]; observer repairs pass
    /// [`InterruptReason::Superseded`] so a repair interrupt is never
    /// classified as a user stop.
    ///
    /// Besides cancelling live tokens, every undelivered inbox row that could
    /// start a turn is withdrawn for the target and each descendant (see
    /// [`Self::withdraw_queued_input`]). A background child is spawned as
    /// "enqueue prompt + notify", so without the withdrawal a stop that lands
    /// before the child actor drains would be a no-op and the child would later
    /// run the queued prompt anyway. The invariant is: once this returns, no
    /// target/descendant starts a *new* turn from work queued before the call.
    ///
    /// The second cancel pass closes the window where an actor became live
    /// between the BFS and the withdrawal and had already promoted its row into
    /// the turn it is running.
    pub async fn interrupt_with_reason(
        &self,
        session_id: &str,
        reason: InterruptReason,
    ) -> Result<bool, RuntimeError> {
        let mut cancelled_any = self.interrupt_single(session_id, reason);
        let mut tree = vec![session_id.to_owned()];
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(session_id.to_owned());
        while let Some(parent) = queue.pop_front() {
            if let Ok(children) = self.inner.config.store.children(&parent).await {
                for child in children {
                    if self.interrupt_single(&child.id, reason) {
                        cancelled_any = true;
                    }
                    queue.push_back(child.id.clone());
                    tree.push(child.id);
                }
            }
        }
        for session in &tree {
            if self.withdraw_queued_input(session).await? {
                cancelled_any = true;
            }
        }
        // Second pass: the actor may have started between the first BFS and the
        // withdrawal above; its row is then already promoted, so only the live
        // token can stop the turn that is now running.
        for session in &tree {
            if self.interrupt_single(session, reason) {
                cancelled_any = true;
            }
        }
        Ok(cancelled_any)
    }

    /// Withdraws every undelivered inbox row the actor would turn into work.
    ///
    /// Withdrawn kinds are exactly the deliverable kinds from
    /// [`Self::next_deliverable`]: `user` (queued prompts), `synthetic`
    /// (observer/compaction prompts), `peer`, `observer` and `compaction`
    /// (queued manual compaction). Both `steer` and `queue` delivery rows are
    /// withdrawn: stop means queued input must not run, and a promoted row is
    /// already a message rather than an inbox row. `move` is left untouched
    /// because no handler exists and it can never become a turn.
    async fn withdraw_queued_input(&self, session: &str) -> Result<bool, RuntimeError> {
        const WITHDRAWN: [&str; 5] = ["user", "synthetic", "peer", "observer", "compaction"];
        let items = self.inner.config.store.list_inbox(session).await?;
        let mut directory: Option<String> = None;
        let mut withdrawn = false;
        for item in items {
            let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
            if !WITHDRAWN.contains(&kind) {
                continue;
            }
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            if self.inner.config.store.cancel_inbox(session, id).await? {
                withdrawn = true;
                let dir = if let Some(dir) = &directory {
                    dir.clone()
                } else {
                    let resolved = self
                        .inner
                        .config
                        .store
                        .get_session(session)
                        .await?
                        .and_then(|value| {
                            value
                                .pointer("/location/directory")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                        .unwrap_or_default();
                    directory = Some(resolved.clone());
                    resolved
                };
                self.emit(
                    session,
                    &dir,
                    oc_types::payload::SessionInboxCancelled {
                        session_id: Runtime::session_id(session),
                        inbox_id: oc_types::MessageId::from_wire(id.to_owned()),
                    },
                )
                .await?;
            }
        }
        Ok(withdrawn)
    }

    fn interrupt_single(&self, session_id: &str, reason: InterruptReason) -> bool {
        let handle = {
            let actors = self.inner.actors.lock().expect("actor map");
            actors.get(session_id).cloned()
        };
        let Some(handle) = handle else {
            return false;
        };
        let token = handle.cancel.lock().expect("cancel slot").clone();
        if let Some(token) = token {
            token.cancel(reason);
            true
        } else {
            false
        }
    }
}
