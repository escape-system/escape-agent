//! LLM-backed memory scribe for the served CLI.
//!
//! The runtime owns the scribe lifecycle but not the model call: the host
//! adapts its provider client to [`oc_memory::ScribeModel`] and hands it over
//! with [`oc_runtime::Runtime::with_scribe`]. One model serves both the
//! `digest` tool and the turn-end memory scribe, so this module resolves the
//! configured digest model once at startup and reuses it per call.

use std::sync::Arc;

use futures_util::StreamExt;
use oc_config::ConfigService;
use oc_llm::{
    Catalog, LlmEvent, LlmMessage, LlmOptions, LlmRequest, Protocol, ResolvedModel, SharedLlmClient,
};
use oc_memory::{BoxFuture, MemoryError, ScribeModel};
use oc_store::Store;
use serde_json::Value;

/// Output ceiling for one scribe completion; a JSON candidate array is short.
const SCRIBE_MAX_TOKENS: u64 = 4096;

/// A [`ScribeModel`] backed by the configured digest model.
#[derive(Debug, Clone)]
pub struct LlmScribe {
    client: Arc<SharedLlmClient>,
    model: ResolvedModel,
    store: Arc<Store>,
}

impl LlmScribe {
    #[must_use]
    pub fn new(client: Arc<SharedLlmClient>, model: ResolvedModel, store: Arc<Store>) -> Self {
        Self {
            client,
            model,
            store,
        }
    }
}

impl ScribeModel for LlmScribe {
    fn complete<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Result<String, MemoryError>> {
        Box::pin(async move {
            let api_key = stored_credential_key(&self.store, &self.model.provider.id).await;
            let messages = vec![LlmMessage::user(prompt)];
            let options = LlmOptions {
                temperature: Some(0.0),
                max_output_tokens: Some(SCRIBE_MAX_TOKENS),
                cache_key: Some("ses_scribe".to_owned()),
                api_key,
                ..LlmOptions::default()
            };
            let mut text = stream_scribe(
                &self.client,
                self.model.clone(),
                messages.clone(),
                options.clone(),
            )
            .await?;
            if text.trim().is_empty() && self.model.variant.is_some() {
                let mut plain = self.model.clone();
                plain.variant = None;
                text = stream_scribe(&self.client, plain, messages, options).await?;
            }
            let text = text.trim();
            if text.is_empty() {
                return Err(MemoryError::Model(
                    "scribe model returned no text".to_owned(),
                ));
            }
            Ok(text.to_owned())
        })
    }
}

async fn stream_scribe(
    client: &SharedLlmClient,
    model: ResolvedModel,
    messages: Vec<LlmMessage>,
    options: LlmOptions,
) -> Result<String, MemoryError> {
    let request = LlmRequest {
        model,
        system: Vec::new(),
        messages,
        tools: Vec::new(),
        options,
    };
    let mut stream = client
        .0
        .stream(request)
        .await
        .map_err(|error| MemoryError::Model(error.to_string()))?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            LlmEvent::TextDelta { delta } => text.push_str(&delta),
            LlmEvent::TextEnd { text: full } => text = full,
            LlmEvent::ProviderError { message } => {
                return Err(MemoryError::Model(message));
            }
            _ => {}
        }
    }
    Ok(text)
}

/// Build the scribe when a digest model resolves.
///
/// Returns `None` and logs once when no reference is configured or the
/// reference does not resolve into a supported protocol. `digest` keeps its
/// explicit "requires a scribe model" error in that case.
pub fn attach(
    config: &ConfigService,
    catalog: &Catalog,
    client: Arc<SharedLlmClient>,
    store: Arc<Store>,
) -> Option<LlmScribe> {
    let reference = digest_reference(config)?;
    let model = match catalog.resolve(&reference) {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(
                model = %reference,
                error = %error,
                "memory scribe disabled: the digest model does not resolve"
            );
            return None;
        }
    };
    if model.protocol == Protocol::Unsupported {
        tracing::warn!(
            model = %reference,
            "memory scribe disabled: unsupported provider protocol"
        );
        return None;
    }
    tracing::info!(
        model = %model.reference,
        "memory scribe attached (digest and turn-end memory extraction)"
    );
    Some(LlmScribe::new(client, model, store))
}

/// Model reference for `digest`: `subagents.digest_model`, then
/// `observer.digest_model`, then `observer.model`.
fn digest_reference(config: &ConfigService) -> Option<String> {
    let merged = config.merged();
    let observer = merged.get("observer");
    // Normalize each candidate before falling through: a present-but-null or
    // empty `subagents.digest_model` (the settings UI writes `null` when the
    // field is cleared) must not stop the chain before `observer.model`.
    let reference = [
        merged
            .get("subagents")
            .and_then(|subagents| subagents.get("digest_model")),
        observer.and_then(|observer| observer.get("digest_model")),
        observer.and_then(|observer| observer.get("model")),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::trim)
    .find(|reference| !reference.is_empty())
    .map(str::to_owned);
    let reference = reference.as_deref();
    let Some(reference) = reference else {
        tracing::warn!(
            "memory scribe disabled: no digest model configured \
             (subagents.digest_model, observer.digest_model, observer.model)"
        );
        return None;
    };
    Some(reference.to_owned())
}

/// Stored credential for `provider`, mirroring the runtime's
/// `credential_key`: the integration's active row wins, otherwise the first
/// non-empty key; OAuth values use `access_token`, other types `key`. Store
/// failures are logged and leave the provider's environment variables to the
/// LLM client.
async fn stored_credential_key(store: &Store, provider: &str) -> Option<String> {
    let credentials = match store.list_credentials().await {
        Ok(credentials) => credentials,
        Err(error) => {
            tracing::warn!(%error, provider, "could not read stored credentials");
            return None;
        }
    };

    let mut fallback = None;
    for entry in credentials {
        if entry.get("integrationID").and_then(Value::as_str) != Some(provider) {
            continue;
        }
        let Some(value) = entry.get("value") else {
            continue;
        };
        let key = match value.get("type").and_then(Value::as_str) {
            Some("oauth") => value.get("access_token").or_else(|| value.get("key")),
            _ => value.get("key").or_else(|| value.get("access_token")),
        }
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty());
        let Some(key) = key else {
            continue;
        };
        if entry.get("active").and_then(Value::as_bool) == Some(true) {
            return Some(key.to_owned());
        }
        fallback.get_or_insert_with(|| key.to_owned());
    }
    fallback
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use oc_config::LoadOptions;
    use oc_store::NewCredential;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use super::*;

    const CATALOG: &str = r#"{
      "test": {
        "id": "test",
        "name": "Test Provider",
        "env": [],
        "models": {
          "alpha": {
            "id": "alpha",
            "type": "chat",
            "name": "Alpha",
            "release_date": "2025-01-02",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "temperature": true},
            "limit": {"context": 100000, "output": 10000},
            "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
            "api": {
              "chat-completions": {
                "base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
              }
            }
          },
          "beta": {
            "id": "beta",
            "type": "chat",
            "name": "Beta",
            "release_date": "2025-01-02",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "temperature": true},
            "limit": {"context": 100000, "output": 10000},
            "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
            "api": {
              "chat-completions": {
                "base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
              }
            }
          }
        }
      }
    }"#;

    fn config_at(directory: &Path, content: &str) -> ConfigService {
        ConfigService::load(
            LoadOptions::new(directory)
                .with_env("HOME", directory.to_string_lossy())
                .with_env("USERPROFILE", directory.to_string_lossy())
                .with_env(
                    "XDG_CONFIG_HOME",
                    directory.join("config").to_string_lossy(),
                )
                .with_env(
                    "ProgramData",
                    directory.join("programdata").to_string_lossy(),
                )
                .with_env(
                    "OPENCODE_CONFIG",
                    directory.join("missing.json").to_string_lossy(),
                )
                .with_env("OPENCODE_CONFIG_CONTENT", content),
        )
        .expect("config loads")
    }

    async fn store_at(directory: &Path) -> Arc<Store> {
        Arc::new(
            Store::open(directory.join("db.sqlite"))
                .await
                .expect("store opens"),
        )
    }

    fn resolved_reference(config: &ConfigService) -> Option<String> {
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let reference = digest_reference(config)?;
        let model = catalog.resolve(&reference).expect("model resolves");
        Some(model.reference)
    }

    #[test]
    fn subagents_digest_model_wins() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{
              "subagents": {"digest_model": "test/beta"},
              "observer": {"digest_model": "test/alpha", "model": "test/alpha"}
            }"#,
        );
        assert_eq!(resolved_reference(&config).as_deref(), Some("test/beta"));
    }

    #[test]
    fn observer_digest_model_beats_observer_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"observer": {"digest_model": "test/beta", "model": "test/alpha"}}"#,
        );
        assert_eq!(resolved_reference(&config).as_deref(), Some("test/beta"));
    }

    #[test]
    fn observer_model_is_the_last_fallback() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"model": "test/alpha"}}"#);
        assert_eq!(resolved_reference(&config).as_deref(), Some("test/alpha"));
    }

    #[test]
    fn null_digest_model_falls_through_to_observer_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"observer": {"digest_model": null, "model": "test/alpha"}}"#,
        );
        assert_eq!(resolved_reference(&config).as_deref(), Some("test/alpha"));
    }

    #[test]
    fn empty_digest_model_falls_through_to_observer_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"subagents": {"digest_model": ""}, "observer": {"model": "test/alpha"}}"#,
        );
        assert_eq!(resolved_reference(&config).as_deref(), Some("test/alpha"));
    }

    #[test]
    fn no_reference_means_no_scribe() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"enabled": true}}"#);
        assert!(digest_reference(&config).is_none());
    }

    #[tokio::test]
    async fn unresolvable_reference_means_no_scribe() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"model": "test/missing"}}"#);
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        assert!(attach(&config, &catalog, client, store).is_none());
    }

    /// OpenAI-compatible SSE server that records the `Authorization` header and
    /// the request body, then streams two deltas.
    struct StreamProvider {
        base_url: String,
        bodies: Arc<Mutex<Vec<Value>>>,
        seen: Arc<Mutex<Vec<Option<String>>>>,
        shutdown: Option<oneshot::Sender<()>>,
        task: Option<JoinHandle<()>>,
    }

    #[derive(Debug)]
    struct StreamState {
        bodies: Arc<Mutex<Vec<Value>>>,
        seen: Arc<Mutex<Vec<Option<String>>>>,
    }

    impl StreamProvider {
        async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("listener binds");
            let address = listener.local_addr().expect("local address");
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let state = Arc::new(StreamState {
                bodies: Arc::clone(&bodies),
                seen: Arc::clone(&seen),
            });
            let app = Router::new()
                .route("/v1/chat/completions", post(stream_chat))
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
                bodies,
                seen,
                shutdown: Some(tx),
                task: Some(task),
            }
        }

        fn base_url(&self) -> &str {
            &self.base_url
        }

        fn bodies(&self) -> Vec<Value> {
            self.bodies.lock().expect("recorded bodies").clone()
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

    impl Drop for StreamProvider {
        fn drop(&mut self) {
            if let Some(task) = self.task.take() {
                task.abort();
            }
        }
    }

    async fn stream_chat(
        State(state): State<Arc<StreamState>>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        state.seen.lock().expect("record authorization").push(
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        );
        state.bodies.lock().expect("record body").push(body);
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
                "choices": [{"index": 0, "delta": {"content": "focused "}, "finish_reason": null}],
            }),
            json!({
                "id": "chatcmpl-fake",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_u64,
                "model": "alpha",
                "choices": [{"index": 0, "delta": {"content": "summary"}, "finish_reason": null}],
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

    fn catalog_pointed_at(base_url: &str) -> Catalog {
        let snapshot = json!({
            "test": {
                "id": "test",
                "name": "Test Provider",
                "env": [],
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
    async fn complete_sends_the_stored_credential_and_concatenates_deltas() {
        let root = TempDir::new().expect("tempdir");
        let store = store_at(root.path()).await;
        // Ids sort ascending, so the inactive row is listed first: only the
        // active flag can select the right key.
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

        let mut provider = StreamProvider::start().await;
        let catalog = catalog_pointed_at(provider.base_url());
        let model = catalog.resolve("test/alpha").expect("model resolves");
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        let scribe = LlmScribe::new(client, model, Arc::clone(&store));

        let text = scribe
            .complete("return the candidates")
            .await
            .expect("complete succeeds");
        assert_eq!(text, "focused summary");
        assert_eq!(
            provider.authorizations(),
            vec![Some("Bearer active-key".to_owned())]
        );

        let bodies = provider.bodies();
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1, "no system message is sent");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "return the candidates");
        assert_eq!(body["max_tokens"], json!(SCRIBE_MAX_TOKENS));
        assert_eq!(body["temperature"], json!(0.0));
        provider.shutdown().await;
    }
}
