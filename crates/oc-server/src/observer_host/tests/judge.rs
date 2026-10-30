//! Judge calls: stored credential selection and the provider request shape.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use oc_llm::{Catalog, SharedLlmClient};
use oc_observer::{ObserverConfig, ObserverModel};
use oc_store::NewCredential;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::super::LlmObserverModel;
use super::store_at;
/// OpenAI-compatible SSE server that records the `Authorization` header
/// and rejects anything but `expected`.
struct VerdictProvider {
    base_url: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct VerdictState {
    expected: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl VerdictProvider {
    async fn start(expected: &str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("listener binds");
        let address = listener.local_addr().expect("local address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(VerdictState {
            expected: expected.to_owned(),
            seen: Arc::clone(&seen),
        });
        let app = Router::new()
            .route("/v1/chat/completions", post(verdict_chat))
            .with_state(state);
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            base_url: format!("http://{address}/v1"),
            seen,
            shutdown: Some(tx),
            task: Some(task),
        }
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn authorizations(&self) -> Vec<Option<String>> {
        self.seen.lock().expect("recorded authorizations").clone()
    }

    async fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(mut task) = self.task.take() {
            let timed_out = tokio::time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err();
            if timed_out {
                task.abort();
            }
        }
    }
}

impl Drop for VerdictProvider {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn verdict_chat(
    State(state): State<Arc<VerdictState>>,
    headers: axum::http::HeaderMap,
    Json(_body): Json<Value>,
) -> axum::response::Response {
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state
        .seen
        .lock()
        .expect("record authorization")
        .push(authorization.clone());
    if authorization.as_deref() != Some(state.expected.as_str()) {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "Missing or invalid Authorization header"}})),
        )
            .into_response();
    }
    let chunks = [
        json!({
            "id": "chatcmpl-fake",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "alpha",
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": null}],
        }),
        json!({
            "id": "chatcmpl-fake",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "alpha",
            "choices": [{
                "index": 0,
                "delta": {"content": r#"{"action":"none"}"#},
                "finish_reason": null,
            }],
        }),
        json!({
            "id": "chatcmpl-fake",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "alpha",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        }),
    ];
    let mut body = String::new();
    for chunk in chunks {
        let _ = write!(body, "data: {chunk}\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/event-stream"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// Catalog whose `test/alpha` provider points at a loopback server.
fn catalog_pointed_at(base_url: &str) -> Catalog {
    let snapshot = json!({
        "test": {
            "id": "test",
            "name": "Test Provider",
            "env": ["OC_OBSERVER_TEST_API_KEY"],
            "models": {
                "alpha": {
                    "id": "alpha",
                    "type": "chat",
                    "name": "Alpha",
                    "release_date": "2025-01-02",
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "capabilities": {"tools": {"supported": true}, "temperature": true},
                    "limit": {"context": 100_000, "output": 10_000},
                    "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
                    "api": {
                        "chat-completions": {
                            "base_url": base_url,
                            "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
                        }
                    }
                }
            }
        }
    });
    Catalog::from_snapshot(&snapshot.to_string()).expect("catalog")
}

#[tokio::test]
async fn judge_uses_stored_credential_and_prefers_the_active_row() {
    assert!(
        std::env::var_os("OC_OBSERVER_TEST_API_KEY").is_none(),
        "test requires the provider credential to stay unset"
    );
    let root = TempDir::new().expect("tempdir");
    let store = store_at(root.path()).await;

    // Ids sort ascending, so `cred_a` (inactive) is listed before
    // `cred_b` (active): only the active flag can select the right key.
    store
        .create_credential(NewCredential {
            id: Some("cred_b".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "key", "key": "active-key"}),
            active: Some(true),
        })
        .await
        .expect("active credential");
    store
        .create_credential(NewCredential {
            id: Some("cred_a".to_owned()),
            integration_id: "test".to_owned(),
            label: None,
            value: json!({"type": "key", "key": "inactive-key"}),
            active: Some(false),
        })
        .await
        .expect("inactive credential");

    let mut provider = VerdictProvider::start("Bearer active-key").await;
    let catalog = catalog_pointed_at(provider.base_url());
    let model = catalog.resolve("test/alpha").expect("model resolves");
    let client = Arc::new(SharedLlmClient::new(
        oc_llm::LlmClient::new().expect("client"),
    ));
    let observer = LlmObserverModel::new(client, model, Arc::clone(&store));

    let verdict = observer
        .judge("{}", &ObserverConfig::default())
        .await
        .expect("judge succeeds with the stored credential");
    assert_eq!(verdict.trim(), r#"{"action":"none"}"#);
    assert_eq!(
        provider.authorizations(),
        vec![Some("Bearer active-key".to_owned())]
    );
    provider.shutdown().await;
}
