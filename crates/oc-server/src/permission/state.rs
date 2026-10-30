//! In-memory permission state: pending requests, saved-rule mirror and the
//! reply state machine.

use std::sync::{Arc, Mutex};

use oc_runtime::{PermissionAsk, PermissionDecision};
use oc_store::Store;
use oc_types::permission::{
    PermissionReply, PermissionRequest, PermissionSource, PermissionSourceKind,
};
use oc_types::{ApiError, EventEnvelope, PermissionRequestId, SessionId};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use super::rules::{durable_rules, persist};
use super::{DECLINED, PendingGuard, not_found};

/// One unresolved question.
pub(super) struct Pending {
    pub(super) id: String,
    pub(super) session_id: String,
    pub(super) action: String,
    pub(super) resources: Vec<String>,
    pub(super) request: Value,
    pub(super) sender: Option<oneshot::Sender<PermissionDecision>>,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pending")
            .field("id", &self.id)
            .field("session_id", &self.session_id)
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}

/// Session-scoped mirror of an `always` reply, used when no durable rule could
/// be written.
#[derive(Debug, Clone)]
struct SavedRule {
    session_id: String,
    action: String,
    resources: Vec<String>,
}

/// Rule view used by coverage checks: one action plus the resources it covers.
/// A `*` resource covers every resource of the same action.
#[derive(Debug, Clone)]
pub(super) struct Rule {
    pub(super) action: String,
    pub(super) resources: Vec<String>,
}

/// Whether `rules` cover every `resource` of `action`.
///
/// Saved rules use the same `*`/`?` glob matching as config and session rules
/// (case-insensitive on Windows, `/`-normalized), so a `*.env` or `*` rule
/// really covers the ask instead of only matching a literal string.
pub(super) fn covered(rules: &[Rule], action: &str, resources: &[String]) -> bool {
    resources.iter().all(|resource| {
        rules.iter().any(|rule| {
            oc_config::wildcard_match(action, &rule.action)
                && rule
                    .resources
                    .iter()
                    .any(|saved| oc_config::wildcard_match(resource, saved))
        })
    })
}

/// `permission.replied` payload.
fn replied(entry: &Pending, reply: PermissionReply) -> Value {
    json!({
        "sessionID": entry.session_id,
        "requestID": entry.id,
        "reply": reply,
    })
}

#[derive(Debug, Default)]
pub(super) struct State {
    pub(super) pending: Mutex<Vec<Pending>>,
    /// In-memory mirror of `always` answers. Only populated when a durable rule
    /// could not be written, so deleting a `psv_` row always takes effect.
    saved: Mutex<Vec<SavedRule>>,
}

impl State {
    pub(super) fn publish(tx: &broadcast::Sender<Arc<EventEnvelope>>, kind: &str, data: Value) {
        let _ = tx.send(Arc::new(EventEnvelope::now(kind, data)));
    }

    /// Rules that apply to `session_id`: the in-memory mirror plus, when a store
    /// is attached, the durable rules of the session's project.
    pub(super) async fn rules(&self, store: Option<&Store>, session_id: &str) -> Vec<Rule> {
        let mut rules = self.memory_rules(session_id);
        if let Some(store) = store {
            match durable_rules(store, session_id).await {
                Ok(durable) => rules.extend(durable),
                Err(error) => tracing::warn!(
                    session = %session_id,
                    error = %error,
                    "saved permission lookup failed"
                ),
            }
        }
        rules
    }

    fn memory_rules(&self, session_id: &str) -> Vec<Rule> {
        self.saved
            .lock()
            .expect("permission saved")
            .iter()
            .filter(|rule| rule.session_id == session_id)
            .map(|rule| Rule {
                action: rule.action.clone(),
                resources: rule.resources.clone(),
            })
            .collect()
    }

    /// Remember an `always` answer whose durable write did not commit.
    fn remember(&self, entry: &Pending) {
        if entry.resources.is_empty() {
            return;
        }
        self.saved
            .lock()
            .expect("permission saved")
            .push(SavedRule {
                session_id: entry.session_id.clone(),
                action: entry.action.clone(),
                resources: entry.resources.clone(),
            });
    }

    async fn resolve_saved(&self, store: Option<&Store>, request: &PermissionAsk) -> bool {
        let rules = self.rules(store, &request.session_id).await;
        covered(&rules, &request.action, &request.resources)
    }

    pub(super) async fn ask(
        self: &Arc<Self>,
        tx: &broadcast::Sender<Arc<EventEnvelope>>,
        store: Option<&Store>,
        request: PermissionAsk,
    ) -> PermissionDecision {
        if self.resolve_saved(store, &request).await {
            return PermissionDecision::Allow;
        }

        let id = PermissionRequestId::new();
        let request_id = id.as_str().to_owned();
        let wire = PermissionRequest {
            id: id.clone(),
            session_id: SessionId::from_wire(request.session_id.clone()),
            action: request.action.clone(),
            resources: request.resources.clone(),
            save: None,
            metadata: None,
            source: Some(PermissionSource {
                kind: PermissionSourceKind::Tool,
                message_id: request.message_id.clone(),
                id: request.tool_call_id.clone(),
            }),
            message: request.message.clone(),
        };
        let value = serde_json::to_value(&wire).expect("permission request is serializable");
        let (sender, receiver) = oneshot::channel();

        {
            let mut pending = self.pending.lock().expect("permission pending");
            pending.push(Pending {
                id: request_id.clone(),
                session_id: request.session_id.clone(),
                action: request.action.clone(),
                resources: request.resources.clone(),
                request: value.clone(),
                sender: Some(sender),
            });
            // Publish while holding the lock so a reply can never observe the
            // request before `permission.asked` reached the bus.
            Self::publish(tx, "permission.asked", value);
        }
        // An interrupted execution drops this future; the guard then removes
        // the request so the UI never lists an unanswerable question.
        let _guard = PendingGuard {
            state: Arc::clone(self),
            id: request_id,
        };

        match receiver.await {
            Ok(decision) => decision,
            Err(_) => PermissionDecision::Deny {
                message: DECLINED.to_owned(),
            },
        }
    }

    pub(super) fn remove(&self, request_id: &str) {
        let mut pending = self.pending.lock().expect("permission pending");
        if let Some(index) = pending.iter().position(|entry| entry.id == request_id) {
            pending.remove(index);
        }
    }

    pub(super) fn reply(
        self: &Arc<Self>,
        tx: &broadcast::Sender<Arc<EventEnvelope>>,
        store: Option<&Arc<Store>>,
        session_id: &str,
        request_id: &str,
        reply: PermissionReply,
        message: Option<String>,
    ) -> Result<(), ApiError> {
        let mut pending = self.pending.lock().expect("permission pending");
        let Some(index) = pending
            .iter()
            .position(|entry| entry.id == request_id && entry.session_id == session_id)
        else {
            return Err(not_found(request_id));
        };
        let mut entry = pending.remove(index);
        let sender = entry.sender.take();
        drop(pending);

        match reply {
            PermissionReply::Reject => {
                // docs/14: `reject` fails the requester and every pending
                // request of that session. Tools in one step run concurrently,
                // so denying only the answered ask would leave its siblings
                // waiting forever (there is no timeout).
                let message = message.unwrap_or_default();
                if let Some(sender) = sender {
                    let _ = sender.send(PermissionDecision::Deny {
                        message: message.clone(),
                    });
                }
                // The answered request is published first, then the siblings
                // it failed (same order as the `always` path).
                Self::publish(tx, "permission.replied", replied(&entry, reply));
                let drained = {
                    let mut pending = self.pending.lock().expect("permission pending");
                    let mut drained = Vec::new();
                    let mut index = 0;
                    while index < pending.len() {
                        if pending[index].session_id == session_id {
                            drained.push(pending.remove(index));
                        } else {
                            index += 1;
                        }
                    }
                    drained
                };
                for mut sibling in drained {
                    if let Some(sender) = sibling.sender.take() {
                        let _ = sender.send(PermissionDecision::Deny {
                            message: message.clone(),
                        });
                    }
                    Self::publish(tx, "permission.replied", replied(&sibling, reply));
                }
            }
            PermissionReply::Once => {
                if let Some(sender) = sender {
                    let _ = sender.send(PermissionDecision::Allow);
                }
                Self::publish(tx, "permission.replied", replied(&entry, reply));
            }
            PermissionReply::Always => {
                if let Some(sender) = sender {
                    let _ = sender.send(PermissionDecision::Allow);
                }
                self.always(tx, store, entry);
            }
        }
        Ok(())
    }

    /// Turn an `always` answer into a durable rule, then publish and
    /// auto-resolve the covered siblings. Falls back to the in-memory mirror
    /// when there is no store or no runtime to write on.
    fn always(
        self: &Arc<Self>,
        tx: &broadcast::Sender<Arc<EventEnvelope>>,
        store: Option<&Arc<Store>>,
        entry: Pending,
    ) {
        let Some(store) = store else {
            tracing::warn!(
                session = %entry.session_id,
                "no store attached: saved permission rule stays in memory"
            );
            self.remember_and_resolve(tx, &entry);
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                session = %entry.session_id,
                "no tokio runtime for the saved permission write: rule stays in memory"
            );
            self.remember_and_resolve(tx, &entry);
            return;
        };
        let state = Arc::clone(self);
        let tx = tx.clone();
        let store = Arc::clone(store);
        handle.spawn(async move {
            // Write first, event second: a received `permission.replied`
            // implies the rule is durable (or, after a failed write, at least
            // that it is held in memory).
            if !persist(&store, &entry).await {
                state.remember(&entry);
            }
            State::publish(
                &tx,
                "permission.replied",
                replied(&entry, PermissionReply::Always),
            );
            let rules = state.rules(Some(&store), &entry.session_id).await;
            state.auto_resolve(&tx, &entry.session_id, &rules);
        });
    }

    /// In-memory `always`: remember the rule, publish the reply and resolve the
    /// covered siblings right away.
    fn remember_and_resolve(&self, tx: &broadcast::Sender<Arc<EventEnvelope>>, entry: &Pending) {
        self.remember(entry);
        Self::publish(
            tx,
            "permission.replied",
            replied(entry, PermissionReply::Always),
        );
        self.auto_resolve(tx, &entry.session_id, &self.memory_rules(&entry.session_id));
    }

    /// Allow every other pending request of `session_id` that `rules` fully
    /// cover, in arrival order, publishing each as an `always` resolution.
    fn auto_resolve(
        &self,
        tx: &broadcast::Sender<Arc<EventEnvelope>>,
        session_id: &str,
        rules: &[Rule],
    ) {
        let mut resolved = Vec::new();
        {
            let mut pending = self.pending.lock().expect("permission pending");
            let mut index = 0;
            while index < pending.len() {
                let entry = &pending[index];
                if entry.session_id == session_id && covered(rules, &entry.action, &entry.resources)
                {
                    let mut entry = pending.remove(index);
                    if let Some(sender) = entry.sender.take() {
                        let _ = sender.send(PermissionDecision::Allow);
                    }
                    resolved.push(entry.id);
                } else {
                    index += 1;
                }
            }
        }
        for request_id in resolved {
            Self::publish(
                tx,
                "permission.replied",
                json!({
                    "sessionID": session_id,
                    "requestID": request_id,
                    "reply": PermissionReply::Always,
                }),
            );
        }
    }

    pub(super) fn pending(&self, session_id: &str) -> Vec<Value> {
        self.pending
            .lock()
            .expect("permission pending")
            .iter()
            .filter(|entry| entry.session_id == session_id)
            .map(|entry| entry.request.clone())
            .collect()
    }

    pub(super) fn request(&self, session_id: &str, request_id: &str) -> Option<Value> {
        self.pending
            .lock()
            .expect("permission pending")
            .iter()
            .find(|entry| entry.id == request_id && entry.session_id == session_id)
            .map(|entry| entry.request.clone())
    }

    pub(super) fn reject_all(&self, tx: &broadcast::Sender<Arc<EventEnvelope>>, message: &str) {
        let drained: Vec<Pending> =
            std::mem::take(&mut *self.pending.lock().expect("permission pending"));
        for mut entry in drained {
            if let Some(sender) = entry.sender.take() {
                let _ = sender.send(PermissionDecision::Deny {
                    message: message.to_owned(),
                });
            }
            Self::publish(
                tx,
                "permission.replied",
                json!({
                    "sessionID": entry.session_id,
                    "requestID": entry.id,
                    "reply": "reject",
                }),
            );
        }
    }
}
