//! Permission broker tests: shared fixtures for the broker, wildcard and
//! store-less fallback suites.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use oc_runtime::PermissionAsk;
use oc_store::{NewSession, Store};
use oc_types::{EventEnvelope, ProjectId, SessionId, now_millis};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::permission::PermissionBroker;

mod broker;
mod fallback;
mod wildcard;

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

async fn create_session(store: &Store, session_id: &str, project_id: &str, dir: &std::path::Path) {
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
async fn next_replied(rx: &mut broadcast::Receiver<Arc<EventEnvelope>>, request_id: &str) -> Value {
    timeout(WAIT, async {
        loop {
            let event = rx.recv().await.expect("bus alive");
            if event.kind == "permission.replied" && event.data["requestID"] == json!(request_id) {
                return event.data.clone();
            }
        }
    })
    .await
    .expect("permission.replied")
}

async fn wait_pending(broker: &PermissionBroker, session_id: &str, expected: usize) -> Vec<Value> {
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
        .find(|entry| entry["action"] == json!(action) && entry["resources"] == json!(resources))
        .and_then(|entry| entry["id"].as_str())
        .expect("pending request")
        .to_owned()
}
