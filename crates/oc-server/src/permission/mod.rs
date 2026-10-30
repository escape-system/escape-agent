//! Permission broker: the server half of the runtime's `Ask` flow.
//!
//! The runtime hands each `ask` rule to [`PermissionBroker::resolve`]. The
//! broker publishes `permission.asked` and waits for the UI to POST a reply.
//! There is deliberately no timeout: an unanswered request waits until the
//! client replies, the execution is interrupted, or the server shuts down.
//!
//! # Saved rules
//!
//! `always` persists every resource of the answered request to the
//! `permission_saved` table for the project owning the session
//! ([`Store::save_permission_rule`]). The write runs on a spawned task and
//! `permission.replied` is published only after it committed, so receiving that
//! event implies the rule is durable. Pending requests of the same session that
//! the new rules fully cover are then allowed without a second reply and
//! published with `reply: "always"` (docs/14: "auto-resolving now-allowed
//! ones"); uncovered requests stay pending.
//!
//! Lookups go through the same table, scoped to the session's project, so a
//! rule saved in one session auto-allows asks in every other session of that
//! project and survives a restart. The in-memory mirror is only populated when
//! a durable rule could not be written (deleting a `psv_` row therefore always
//! takes effect): a broker without a store ([`PermissionBroker::new`], used by
//! tests and embedders) keeps saved rules in memory keyed by session for the
//! broker's lifetime only, and the same fallback applies when a durable write
//! cannot run.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use oc_runtime::{PermissionAsk, PermissionDecision, PermissionHandler};
use oc_store::Store;
use oc_types::permission::PermissionReply;
use oc_types::{ApiError, EventEnvelope};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

use state::{Pending, State, covered};

mod rules;
mod state;

/// Message carried by a `Deny` when the host tears the request down.
const DECLINED: &str = "Permission request was declined because the server is shutting down.";

/// Shared broker handle. Cloning is cheap; dropping the last handle rejects
/// every pending request with a deny.
#[derive(Debug, Clone)]
pub struct PermissionBroker {
    inner: Arc<Inner>,
}

struct Inner {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
    store: Option<Arc<Store>>,
    state: Arc<State>,
}

/// Removes a pending request when its resolver future is dropped, which is how
/// an interrupted execution abandons an unanswered question.
struct PendingGuard {
    state: Arc<State>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state.remove(&self.id);
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PermissionBroker")
            .field("pending", &self.state.pending.lock().map_or(0, |p| p.len()))
            .finish_non_exhaustive()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.state.reject_all(&self.tx, DECLINED);
    }
}

impl PermissionBroker {
    /// Broker without a store: `always` rules apply to their session from an
    /// in-memory mirror for the lifetime of the broker and are lost on restart.
    #[must_use]
    pub fn new(tx: broadcast::Sender<Arc<EventEnvelope>>) -> Self {
        Self::build(tx, None)
    }

    /// Broker that persists `always` rules to `permission_saved`.
    #[must_use]
    pub fn with_store(tx: broadcast::Sender<Arc<EventEnvelope>>, store: Arc<Store>) -> Self {
        Self::build(tx, Some(store))
    }

    /// Build a broker that publishes on an existing server state's bus and
    /// persists `always` rules through its store.
    #[must_use]
    pub fn from_state(state: &crate::AppState) -> Self {
        Self::build(state.bus().clone(), state.store().cloned())
    }

    fn build(tx: broadcast::Sender<Arc<EventEnvelope>>, store: Option<Arc<Store>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                tx,
                store,
                state: Arc::new(State::default()),
            }),
        }
    }

    /// Pending requests for one session, in arrival order.
    #[must_use]
    pub fn pending(&self, session_id: &str) -> Vec<Value> {
        self.inner.state.pending(session_id)
    }

    /// Whether the saved rules that apply to `session_id` cover every
    /// `resource` of `action`. Used by the admission route so a rule saved by
    /// `always` also answers a direct `POST .../permission` instead of parking
    /// a request nobody awaits.
    pub async fn covers_saved(&self, session_id: &str, action: &str, resources: &[String]) -> bool {
        let rules = self
            .inner
            .state
            .rules(self.inner.store.as_deref(), session_id)
            .await;
        covered(&rules, action, resources)
    }

    /// One pending request, or `None` after it was answered.
    #[must_use]
    pub fn request(&self, session_id: &str, request_id: &str) -> Option<Value> {
        self.inner.state.request(session_id, request_id)
    }

    /// Register a request created by `POST .../permission` rather than by a
    /// running tool. Nothing awaits the decision; a reply simply clears it.
    pub fn enqueue(
        &self,
        session_id: &str,
        id: &str,
        action: String,
        resources: Vec<String>,
        request: Value,
    ) {
        let (sender, _receiver) = oneshot::channel();
        {
            let mut pending = self.inner.state.pending.lock().expect("permission pending");
            pending.push(Pending {
                id: id.to_owned(),
                session_id: session_id.to_owned(),
                action,
                resources,
                request: request.clone(),
                sender: Some(sender),
            });
            State::publish(&self.inner.tx, "permission.asked", request);
        }
    }

    /// Resolve one request. Returns `PermissionNotFoundError` when it is not
    /// pending for this session.
    ///
    /// `always` returns once the asker was allowed; the saved rule is written
    /// on a spawned task and `permission.replied` follows the commit.
    pub fn reply(
        &self,
        session_id: &str,
        request_id: &str,
        reply: PermissionReply,
        message: Option<String>,
    ) -> Result<(), ApiError> {
        self.inner.state.reply(
            &self.inner.tx,
            self.inner.store.as_ref(),
            session_id,
            request_id,
            reply,
            message,
        )
    }

    /// Reject every pending request; used by shutdown paths that cannot wait
    /// for the last handle to drop.
    pub fn reject_all(&self, message: &str) {
        self.inner.state.reject_all(&self.inner.tx, message);
    }
}

impl PermissionHandler for PermissionBroker {
    fn resolve(&self, request: PermissionAsk) -> BoxFuture<'static, PermissionDecision> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            inner
                .state
                .ask(&inner.tx, inner.store.as_deref(), request)
                .await
        })
    }
}

fn not_found(request_id: &str) -> ApiError {
    ApiError::not_found(
        "PermissionNotFoundError",
        format!("Permission request not found: {request_id}"),
    )
    .with("requestID", json!(request_id))
}

#[cfg(test)]
mod tests;
