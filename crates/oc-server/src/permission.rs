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

use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use oc_runtime::{PermissionAsk, PermissionDecision, PermissionHandler};
use oc_store::{Store, StoreError};
use oc_types::permission::{
    PermissionReply, PermissionRequest, PermissionSource, PermissionSourceKind,
};
use oc_types::{ApiError, EventEnvelope, PermissionRequestId, SessionId};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

/// Message carried by a `Deny` when the host tears the request down.
const DECLINED: &str = "Permission request was declined because the server is shutting down.";

/// One unresolved question.
struct Pending {
    id: String,
    session_id: String,
    action: String,
    resources: Vec<String>,
    request: Value,
    sender: Option<oneshot::Sender<PermissionDecision>>,
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
struct Rule {
    action: String,
    resources: Vec<String>,
}

/// Whether `rules` cover every `resource` of `action`.
///
/// Saved rules use the same `*`/`?` glob matching as config and session rules
/// (case-insensitive on Windows, `/`-normalized), so a `*.env` or `*` rule
/// really covers the ask instead of only matching a literal string.
fn covered(rules: &[Rule], action: &str, resources: &[String]) -> bool {
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

/// Project owning `session_id`, read from the `Session.Info.projectID` column.
async fn project_of(store: &Store, session_id: &str) -> Result<Option<String>, StoreError> {
    Ok(store.get_session(session_id).await?.and_then(|session| {
        session
            .get("projectID")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }))
}

/// Durable saved rules of the project owning `session_id`.
async fn durable_rules(store: &Store, session_id: &str) -> Result<Vec<Rule>, StoreError> {
    let Some(project_id) = project_of(store, session_id).await? else {
        return Ok(Vec::new());
    };
    let saved = store.list_saved_permissions(Some(&project_id)).await?;
    Ok(saved
        .iter()
        .filter_map(|row| {
            Some(Rule {
                action: row.get("action")?.as_str()?.to_owned(),
                resources: vec![row.get("resource")?.as_str()?.to_owned()],
            })
        })
        .collect())
}

/// Writes every resource of an `always` answer to the session's project.
/// Returns whether all writes committed; the caller mirrors the rule in memory
/// otherwise.
async fn persist(store: &Store, entry: &Pending) -> bool {
    let project_id = match project_of(store, &entry.session_id).await {
        Ok(Some(project_id)) => project_id,
        Ok(None) => {
            tracing::warn!(
                session = %entry.session_id,
                "saved permission rule kept in memory: the session row is missing"
            );
            return false;
        }
        Err(error) => {
            tracing::warn!(
                session = %entry.session_id,
                error = %error,
                "saved permission rule kept in memory: the session lookup failed"
            );
            return false;
        }
    };
    let mut persisted = true;
    for resource in &entry.resources {
        if let Err(error) = store
            .save_permission_rule(&project_id, &entry.action, resource)
            .await
        {
            tracing::warn!(
                project = %project_id,
                action = %entry.action,
                resource = %resource,
                error = %error,
                "saved permission write failed"
            );
            persisted = false;
        }
    }
    persisted
}

#[derive(Debug, Default)]
struct State {
    pending: Mutex<Vec<Pending>>,
    /// In-memory mirror of `always` answers. Only populated when a durable rule
    /// could not be written, so deleting a `psv_` row always takes effect.
    saved: Mutex<Vec<SavedRule>>,
}

impl State {
    fn publish(tx: &broadcast::Sender<Arc<EventEnvelope>>, kind: &str, data: Value) {
        let _ = tx.send(Arc::new(EventEnvelope::now(kind, data)));
    }

    /// Rules that apply to `session_id`: the in-memory mirror plus, when a store
    /// is attached, the durable rules of the session's project.
    async fn rules(&self, store: Option<&Store>, session_id: &str) -> Vec<Rule> {
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

    async fn ask(
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

    fn remove(&self, request_id: &str) {
        let mut pending = self.pending.lock().expect("permission pending");
        if let Some(index) = pending.iter().position(|entry| entry.id == request_id) {
            pending.remove(index);
        }
    }

    fn reply(
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

    fn pending(&self, session_id: &str) -> Vec<Value> {
        self.pending
            .lock()
            .expect("permission pending")
            .iter()
            .filter(|entry| entry.session_id == session_id)
            .map(|entry| entry.request.clone())
            .collect()
    }

    fn request(&self, session_id: &str, request_id: &str) -> Option<Value> {
        self.pending
            .lock()
            .expect("permission pending")
            .iter()
            .find(|entry| entry.id == request_id && entry.session_id == session_id)
            .map(|entry| entry.request.clone())
    }

    fn reject_all(&self, tx: &broadcast::Sender<Arc<EventEnvelope>>, message: &str) {
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
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use oc_store::NewSession;
    use oc_types::{ProjectId, now_millis};
    use tempfile::TempDir;
    use tokio::time::timeout;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    struct Fixture {
        _dir: TempDir,
        path: PathBuf,
        store: Arc<Store>,
        project_id: String,
        session_id: String,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("data.sqlite");
            let store = Arc::new(Store::open(&path).await.expect("store"));
            let project_id = ProjectId::new().into_inner();
            let session_id = SessionId::new().into_inner();
            create_session(&store, &session_id, &project_id, dir.path()).await;
            Self {
                _dir: dir,
                path,
                store,
                project_id,
                session_id,
            }
        }

        fn broker(&self) -> (PermissionBroker, broadcast::Receiver<Arc<EventEnvelope>>) {
            let (tx, rx) = broadcast::channel(64);
            (
                PermissionBroker::with_store(tx, Arc::clone(&self.store)),
                rx,
            )
        }
    }

    async fn create_session(
        store: &Store,
        session_id: &str,
        project_id: &str,
        dir: &std::path::Path,
    ) {
        store
            .create_session(NewSession {
                id: session_id.to_owned(),
                parent_id: None,
                project_id: project_id.to_owned(),
                directory: dir.to_string_lossy().into_owned(),
                title: None,
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                time_created: now_millis(),
            })
            .await
            .expect("session");
    }

    fn ask(session_id: &str, action: &str, resources: &[&str]) -> PermissionAsk {
        PermissionAsk {
            session_id: session_id.to_owned(),
            message_id: "msg_test".to_owned(),
            tool_call_id: "call_test".to_owned(),
            tool: "shell".to_owned(),
            action: action.to_owned(),
            resources: resources
                .iter()
                .map(|resource| (*resource).to_owned())
                .collect(),
            message: None,
        }
    }

    /// Waits for `permission.replied` about `request_id` and returns its data.
    async fn next_replied(
        rx: &mut broadcast::Receiver<Arc<EventEnvelope>>,
        request_id: &str,
    ) -> Value {
        timeout(WAIT, async {
            loop {
                let event = rx.recv().await.expect("bus alive");
                if event.kind == "permission.replied"
                    && event.data["requestID"] == json!(request_id)
                {
                    return event.data.clone();
                }
            }
        })
        .await
        .expect("permission.replied")
    }

    async fn wait_pending(
        broker: &PermissionBroker,
        session_id: &str,
        expected: usize,
    ) -> Vec<Value> {
        timeout(WAIT, async {
            loop {
                let pending = broker.pending(session_id);
                if pending.len() >= expected {
                    return pending;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("pending requests")
    }

    fn request_id(pending: &[Value], action: &str, resources: &[&str]) -> String {
        pending
            .iter()
            .find(|entry| {
                entry["action"] == json!(action) && entry["resources"] == json!(resources)
            })
            .and_then(|entry| entry["id"].as_str())
            .expect("pending request")
            .to_owned()
    }

    #[tokio::test]
    async fn always_persists_the_rule_and_lists_it() {
        let fixture = Fixture::new().await;
        let (broker, mut events) = fixture.broker();
        let session = fixture.session_id.clone();
        let enqueued = |id: &str, resources: &[&str]| {
            json!({
                "id": id,
                "sessionID": session,
                "action": "shell",
                "resources": resources,
            })
        };
        broker.enqueue(
            &session,
            "per_echo",
            "shell".to_owned(),
            vec!["echo ok".to_owned()],
            enqueued("per_echo", &["echo ok"]),
        );
        broker.enqueue(
            &session,
            "per_rm",
            "shell".to_owned(),
            vec!["rm x".to_owned()],
            enqueued("per_rm", &["rm x"]),
        );
        broker
            .reply(&session, "per_echo", PermissionReply::Always, None)
            .expect("reply");

        // The event follows the committed write, so the row must exist now.
        let event = next_replied(&mut events, "per_echo").await;
        assert_eq!(event["reply"], json!("always"));
        let saved = fixture
            .store
            .list_saved_permissions(Some(&fixture.project_id))
            .await
            .expect("saved rules");
        assert!(
            saved
                .iter()
                .any(|row| row["action"] == json!("shell") && row["resource"] == json!("echo ok")),
            "saved rules: {saved:?}"
        );

        // Only the covered reply was answered; `rm x` is still pending.
        let pending = broker.pending(&session);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["id"], json!("per_rm"));
        drop(broker);
        drop(pending);

        // Restart simulation: a fresh store handle and a fresh broker.
        let project_id = fixture.project_id.clone();
        let second_session = SessionId::new().into_inner();
        create_session(
            &fixture.store,
            &second_session,
            &project_id,
            fixture.path.parent().expect("dir"),
        )
        .await;
        drop(fixture.store);
        let restarted = Arc::new(Store::open(&fixture.path).await.expect("reopened store"));
        let (tx, _rx) = broadcast::channel(8);
        let broker = PermissionBroker::with_store(tx, Arc::clone(&restarted));
        let decision = timeout(WAIT, broker.resolve(ask(&session, "shell", &["echo ok"])))
            .await
            .expect("durable resolve");
        assert_eq!(decision, PermissionDecision::Allow);
        // Another session of the same project is auto-allowed as well.
        let decision = timeout(
            WAIT,
            broker.resolve(ask(&second_session, "shell", &["echo ok"])),
        )
        .await
        .expect("cross-session resolve");
        assert_eq!(decision, PermissionDecision::Allow);
        // The uncovered resource is not: it raises a fresh request.
        let uncovered = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "shell", &["rm x"])).await }
        });
        let pending = wait_pending(&broker, &session, 1).await;
        let id = request_id(&pending, "shell", &["rm x"]);
        broker
            .reply(&session, &id, PermissionReply::Reject, None)
            .expect("reject");
        assert!(matches!(
            timeout(WAIT, uncovered)
                .await
                .expect("uncovered")
                .expect("join"),
            PermissionDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn always_auto_resolves_covered_pending_requests_in_the_same_session() {
        let fixture = Fixture::new().await;
        let (broker, mut events) = fixture.broker();
        let session = fixture.session_id.clone();
        let star = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "shell", &["*"])).await }
        });
        let covered_ask = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "shell", &["echo ok"])).await }
        });
        let uncovered_ask = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "read", &["notes.txt"])).await }
        });

        let pending = wait_pending(&broker, &session, 3).await;
        let star_id = request_id(&pending, "shell", &["*"]);
        let covered_id = request_id(&pending, "shell", &["echo ok"]);
        let uncovered_id = request_id(&pending, "read", &["notes.txt"]);
        broker
            .reply(&session, &star_id, PermissionReply::Always, None)
            .expect("reply");

        assert_eq!(
            timeout(WAIT, star).await.expect("star").expect("join"),
            PermissionDecision::Allow
        );
        // The sibling covered by the `*` rule is allowed without a reply.
        assert_eq!(
            timeout(WAIT, covered_ask)
                .await
                .expect("covered")
                .expect("join"),
            PermissionDecision::Allow
        );
        let event = next_replied(&mut events, &covered_id).await;
        assert_eq!(event["reply"], json!("always"));
        // A resource of another action is not covered and stays pending.
        let pending = broker.pending(&session);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["id"], json!(uncovered_id));

        broker
            .reply(&session, &uncovered_id, PermissionReply::Reject, None)
            .expect("reject");
        assert!(matches!(
            timeout(WAIT, uncovered_ask)
                .await
                .expect("uncovered")
                .expect("join"),
            PermissionDecision::Deny { .. }
        ));
        assert!(broker.pending(&session).is_empty());
    }

    #[test]
    fn covered_uses_wildcard_rules() {
        let rules = vec![Rule {
            action: "read".to_owned(),
            resources: vec!["*.env".to_owned()],
        }];
        assert!(covered(&rules, "read", &["config/.env".to_owned()]));
        assert!(!covered(&rules, "read", &["config/.env.local".to_owned()]));
        assert!(!covered(&rules, "write", &["config/.env".to_owned()]));
        let star = vec![Rule {
            action: "*".to_owned(),
            resources: vec!["*".to_owned()],
        }];
        assert!(covered(&star, "shell", &["rm x".to_owned()]));
    }

    #[tokio::test]
    async fn reject_denies_every_pending_request_of_the_session() {
        let fixture = Fixture::new().await;
        let (broker, mut events) = fixture.broker();
        let session = fixture.session_id.clone();
        let other_session = SessionId::new().into_inner();
        create_session(
            &fixture.store,
            &other_session,
            &fixture.project_id,
            std::path::Path::new("."),
        )
        .await;

        let first = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "shell", &["echo a"])).await }
        });
        let second = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "shell", &["echo b"])).await }
        });
        let other = tokio::spawn({
            let broker = broker.clone();
            let session = other_session.clone();
            async move { broker.resolve(ask(&session, "read", &["notes.txt"])).await }
        });

        let pending = wait_pending(&broker, &session, 2).await;
        let first_id = request_id(&pending, "shell", &["echo a"]);
        let second_id = request_id(&pending, "shell", &["echo b"]);
        wait_pending(&broker, &other_session, 1).await;

        broker
            .reply(
                &session,
                &first_id,
                PermissionReply::Reject,
                Some("no".to_owned()),
            )
            .expect("reject");

        // Both asks of the rejected session fail; the other session keeps its
        // pending request.
        assert!(matches!(
            timeout(WAIT, first).await.expect("first").expect("join"),
            PermissionDecision::Deny { .. }
        ));
        assert!(matches!(
            timeout(WAIT, second).await.expect("second").expect("join"),
            PermissionDecision::Deny { .. }
        ));
        assert!(broker.pending(&session).is_empty());
        assert_eq!(broker.pending(&other_session).len(), 1);

        let event = next_replied(&mut events, &first_id).await;
        assert_eq!(event["reply"], json!("reject"));
        let event = next_replied(&mut events, &second_id).await;
        assert_eq!(event["reply"], json!("reject"));

        // Release the untouched ask so the broker drop does not race the test.
        broker.reject_all("done");
        assert!(matches!(
            timeout(WAIT, other).await.expect("other").expect("join"),
            PermissionDecision::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn always_without_a_store_keeps_the_in_memory_fallback() {
        let (tx, mut events) = broadcast::channel(8);
        let broker = PermissionBroker::new(tx);
        let session = SessionId::new().into_inner();
        let first = tokio::spawn({
            let broker = broker.clone();
            let session = session.clone();
            async move { broker.resolve(ask(&session, "shell", &["echo ok"])).await }
        });
        let pending = wait_pending(&broker, &session, 1).await;
        let id = request_id(&pending, "shell", &["echo ok"]);
        broker
            .reply(&session, &id, PermissionReply::Always, None)
            .expect("reply");
        assert_eq!(
            timeout(WAIT, first).await.expect("ask").expect("join"),
            PermissionDecision::Allow
        );
        let event = next_replied(&mut events, &id).await;
        assert_eq!(event["reply"], json!("always"));

        // The in-memory rule answers the next ask without a pending request.
        assert_eq!(
            timeout(WAIT, broker.resolve(ask(&session, "shell", &["echo ok"])))
                .await
                .expect("memory resolve"),
            PermissionDecision::Allow
        );
        assert!(broker.pending(&session).is_empty());
    }
}
