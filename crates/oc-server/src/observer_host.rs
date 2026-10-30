//! Host implementations of the `oc-observer` collaborators.
//!
//! The observer loop lives in `oc-observer`; this module supplies the three
//! pieces it cannot own:
//!
//! - [`LlmObserverModel`] performs one strict, tool-free provider call and
//!   returns the raw JSON verdict text.
//! - [`BusObserverNotifier`] publishes `observer.*`/`goal.updated` events,
//!   persists the repair audit row and goal transitions, and rate-limits
//!   unavailable notices.
//! - [`build_for_state`] reads `observer.*` from the merged configuration,
//!   resolves the model and assembles an [`ObserverHandle`] the CLI attaches
//!   to the runtime.
//!
//! The observer is opt-in: without `observer.model` (or a digest-model
//! fallback) `build_for_state` returns `None` and logs one line. Credentials
//! are resolved per request — first from the store, then (inside the LLM
//! client) from the provider's environment variables — so a model whose
//! provider declares an environment credential still builds without that
//! variable, and a judge call without any usable credential fails open and
//! publishes `observer.unavailable`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use oc_config::ConfigService;
use oc_llm::{
    Catalog, LlmEvent, LlmMessage, LlmOptions, LlmRequest, Protocol, ResolvedModel, SharedLlmClient,
};
use oc_observer::{
    BoxFuture, GoalConfig, GoalSnapshot, GoalStore, ObserverConfig, ObserverEngine, ObserverError,
    ObserverModel, ObserverNotice, ObserverNotifier, SystemClock,
};
use oc_runtime::{RepairChannel, RepairCommand, StoreGoalStore};
use oc_store::{GoalPatch, NewRepair, Store};
use oc_types::{EventEnvelope, EventId, EventLocation};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::AppState;

/// Maximum tokens the observer may spend on one verdict.
const VERDICT_MAX_TOKENS: u64 = 512;

/// Minimum gap between two `observer.unavailable` events per root.
const UNAVAILABLE_NOTICE_INTERVAL_MS: i64 = 300_000;

/// The model half of the observer: one strict JSON request per tick.
#[derive(Debug)]
pub struct LlmObserverModel {
    client: Arc<SharedLlmClient>,
    model: ResolvedModel,
    store: Arc<Store>,
}

impl LlmObserverModel {
    /// Build a model over an already resolved provider/model pair.
    #[must_use]
    pub fn new(client: Arc<SharedLlmClient>, model: ResolvedModel, store: Arc<Store>) -> Self {
        Self {
            client,
            model,
            store,
        }
    }
}

/// Stored credential for `provider`, mirroring the runtime's private
/// `credential_key` (`oc-runtime/src/turn.rs`): the integration's active row
/// wins, otherwise the first non-empty key; OAuth values use `access_token`,
/// other types `key`. Store failures are logged and leave the provider's
/// environment variables to the LLM client.
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

impl ObserverModel for LlmObserverModel {
    fn judge<'a>(
        &'a self,
        input: &'a str,
        cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>> {
        Box::pin(async move {
            let api_key = stored_credential_key(&self.store, &self.model.provider.id).await;
            let request = LlmRequest {
                model: self.model.clone(),
                system: vec![system_prompt(cfg.nudge_max_chars)],
                messages: vec![LlmMessage::user(input)],
                tools: Vec::new(),
                options: LlmOptions {
                    temperature: Some(0.0),
                    max_output_tokens: Some(VERDICT_MAX_TOKENS),
                    api_key,
                    ..LlmOptions::default()
                },
            };
            let mut stream = self
                .client
                .0
                .stream(request)
                .await
                .map_err(|error| ObserverError::Model(error.to_string()))?;
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    LlmEvent::TextDelta { delta } => text.push_str(&delta),
                    LlmEvent::TextEnd { text: full } => text = full,
                    LlmEvent::ProviderError { message } => {
                        return Err(ObserverError::Model(message));
                    }
                    _ => {}
                }
            }
            if text.trim().is_empty() {
                return Err(ObserverError::Model(
                    "observer model returned no verdict text".to_owned(),
                ));
            }
            Ok(text)
        })
    }
}

/// System prompt: static contract only, no transcript or tool data.
fn system_prompt(nudge_max_chars: usize) -> String {
    format!(
        "You are a supervisor. Return ONLY the JSON verdict described. Do not use tools.\n\n\
         The user message is one compact JSON document about a running session tree \
         (goal, root, active strips, recent strips, tree). Decide whether to intervene.\n\
         Reply with exactly one JSON object and nothing else:\n\
         {{\"action\":\"none|nudge|inspect|repair|goal_done|goal_blocked|goal_failed\",\
         \"target\":\"session or tool-call id (required except for none)\",\
         \"reason\":\"why, at most 20 words\",\
         \"message\":\"nudge text, at most {nudge_max_chars} chars (nudge only)\",\
         \"repair\":{{\"kind\":\"retry_tool|cancel_tool|restart_subagent|compact_context|switch_model|repost\",\"args\":{{}}}},\
         \"goal_evidence\":[\"tool_call_id or message_id\"]}}"
    )
}

/// Publishes observer events and persists repair audit rows and goal
/// transitions.
pub struct BusObserverNotifier {
    tx: broadcast::Sender<Arc<EventEnvelope>>,
    store: Arc<Store>,
    directory: Arc<str>,
    last_unavailable: Mutex<HashMap<String, i64>>,
}

impl BusObserverNotifier {
    /// Build a notifier over the server bus and the shared store.
    #[must_use]
    pub fn new(
        tx: broadcast::Sender<Arc<EventEnvelope>>,
        store: Arc<Store>,
        directory: Arc<str>,
    ) -> Self {
        Self {
            tx,
            store,
            directory,
            last_unavailable: Mutex::new(HashMap::new()),
        }
    }

    /// Persist the audit row, then publish `observer.repaired`.
    fn report_repair(
        &self,
        root_id: &str,
        target: &str,
        kind: &'static str,
        reason: &str,
        attempt: u32,
        budget_left: u32,
    ) {
        let record = NewRepair {
            id: repair_id(),
            root_id: root_id.to_owned(),
            target_id: target.to_owned(),
            kind: kind.to_owned(),
            reason: reason.to_owned(),
            args_hash: repair_args_hash(kind, target, reason),
            attempt,
            time_created: oc_types::now_millis(),
        };
        let payload = json!({
            "rootID": root_id,
            "target": target,
            "kind": kind,
            "reason": reason,
            "attempt": attempt,
            "budgetLeft": budget_left,
        });
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                root = %record.root_id,
                "observer.repaired dropped: no tokio runtime for the audit write"
            );
            return;
        };
        let tx = self.tx.clone();
        let store = Arc::clone(&self.store);
        let directory = Arc::clone(&self.directory);
        let log_root = record.root_id.clone();
        handle.spawn(async move {
            // Audit first, event second: a received event implies the repair
            // row exists.
            if let Err(error) = store.record_repair(record).await {
                tracing::warn!(
                    root = %log_root,
                    error = %error,
                    "observer repair audit write failed"
                );
                return;
            }
            publish(&tx, &directory, "observer.repaired", payload);
        });
    }

    /// Persist the goal transition, then publish `goal.updated`.
    ///
    /// A received event therefore implies the goal row carries the
    /// transitioned status, rounds, stalls, progress clock and evidence.
    fn report_goal(
        &self,
        root_id: &str,
        goal_id: &str,
        status: &'static str,
        reason: &str,
        goal: &GoalSnapshot,
    ) {
        // `GoalPatch` has no columns for `wait_until_ms` / `wait_started_ms`;
        // those clocks stay engine-only (see `StoreGoalStore::save`).
        let patch = GoalPatch {
            status: Some(status.to_owned()),
            rounds: Some(goal.rounds),
            stalls: Some(goal.stalls),
            last_progress: Some(goal.last_progress_ms),
            evidence: Some(goal.evidence.clone()),
        };
        let payload = json!({
            "rootID": root_id,
            "goalID": goal_id,
            "status": status,
            "reason": reason,
            "goal": goal,
        });
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                root = %root_id,
                goal = %goal_id,
                "goal.updated dropped: no tokio runtime for the goal write"
            );
            return;
        };
        let tx = self.tx.clone();
        let store = Arc::clone(&self.store);
        let directory = Arc::clone(&self.directory);
        let goal_id = goal_id.to_owned();
        let log_root = root_id.to_owned();
        let log_goal = goal_id.clone();
        handle.spawn(async move {
            // Write first, event second: a received event implies the row was
            // committed.
            match store.update_goal(&goal_id, patch).await {
                Ok(Some(_)) => publish(&tx, &directory, "goal.updated", payload),
                Ok(None) => tracing::warn!(
                    root = %log_root,
                    goal = %log_goal,
                    "goal.updated dropped: goal row no longer exists"
                ),
                Err(error) => tracing::warn!(
                    root = %log_root,
                    goal = %log_goal,
                    %error,
                    "goal update write failed"
                ),
            }
        });
    }
}

impl fmt::Debug for BusObserverNotifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BusObserverNotifier")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl ObserverNotifier for BusObserverNotifier {
    fn notify(&self, event: ObserverNotice) {
        match event {
            ObserverNotice::Repaired {
                root_id,
                target,
                kind,
                reason,
                attempt,
                budget_left,
            } => self.report_repair(
                &root_id,
                &target,
                kind.as_str(),
                &reason,
                attempt,
                budget_left,
            ),
            ObserverNotice::Escalated {
                root_id,
                reason,
                budget,
            } => publish(
                &self.tx,
                &self.directory,
                "observer.escalated",
                json!({"rootID": root_id, "reason": reason, "budget": budget}),
            ),
            ObserverNotice::Unavailable { root_id, reason } => {
                let now = oc_types::now_millis();
                let mut seen = self
                    .last_unavailable
                    .lock()
                    .expect("observer unavailable clock");
                if seen
                    .get(&root_id)
                    .is_some_and(|last| now.saturating_sub(*last) < UNAVAILABLE_NOTICE_INTERVAL_MS)
                {
                    return;
                }
                seen.insert(root_id.clone(), now);
                drop(seen);
                publish(
                    &self.tx,
                    &self.directory,
                    "observer.unavailable",
                    json!({"rootID": root_id, "reason": reason}),
                );
            }
            ObserverNotice::Goal {
                root_id,
                goal_id,
                status,
                reason,
                goal,
            } => self.report_goal(&root_id, &goal_id, status.as_str(), &reason, &goal),
            ObserverNotice::Nudged { .. } | ObserverNotice::Inspected { .. } => {
                // The runtime applies nudges/inspects; publishing here would
                // duplicate its effects.
            }
            ObserverNotice::RepairFailed {
                root_id,
                target,
                kind,
                reason,
            } => tracing::warn!(
                root = %root_id,
                target = %target,
                kind = kind.as_str(),
                %reason,
                "observer repair failed"
            ),
        }
    }
}

fn publish(tx: &broadcast::Sender<Arc<EventEnvelope>>, directory: &str, kind: &str, data: Value) {
    let mut envelope = EventEnvelope::now(kind, data);
    envelope.location = Some(EventLocation {
        directory: directory.to_owned(),
    });
    // A send error only means no subscriber is connected.
    let _ = tx.send(Arc::new(envelope));
}

/// Stable hash of the repair context. Verdict arguments are not available to
/// the notifier, so kind, target and reason stand in for them.
fn repair_args_hash(kind: &str, target: &str, reason: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(target.as_bytes());
    hasher.update(b"\0");
    hasher.update(reason.as_bytes());
    hex::encode(hasher.finalize())
}

fn repair_id() -> String {
    // Reuse the event-id generator; `obr_` marks an observer repair row.
    EventId::new().into_inner().replacen("evt_", "obr_", 1)
}

/// Collaborators the CLI attaches to [`oc_runtime::Runtime`].
pub struct ObserverHandle {
    /// The observer loop.
    pub engine: Arc<ObserverEngine>,
    /// Goal persistence shared with the runtime ticker.
    pub goals: Arc<dyn GoalStore>,
    /// Repairs produced by the engine executor.
    pub repairs: UnboundedReceiver<RepairCommand>,
}

impl fmt::Debug for ObserverHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObserverHandle")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// Build the observer host from the merged configuration.
///
/// Returns `None` when the observer is disabled or has no usable model
/// reference, logging exactly once per startup attempt.
#[must_use]
pub fn build_for_state(
    state: &AppState,
    store: Arc<Store>,
    config: &ConfigService,
    catalog: &Catalog,
    client: Arc<SharedLlmClient>,
) -> Option<ObserverHandle> {
    let observer_config = parse_observer_config(config)?;
    let model = resolve_model(config, catalog, &observer_config)?;
    let (executor, repairs) = RepairChannel::new();
    let directory = state.directory().to_string_lossy().into_owned();
    let notifier = Arc::new(BusObserverNotifier::new(
        state.bus().clone(),
        Arc::clone(&store),
        Arc::from(directory.as_str()),
    ));
    let engine = Arc::new(ObserverEngine::new(
        observer_config,
        Arc::new(SystemClock),
        Arc::new(LlmObserverModel::new(client, model, Arc::clone(&store))),
        executor,
        notifier,
    ));
    let goals: Arc<dyn GoalStore> =
        Arc::new(StoreGoalStore::new(store).with_policy(parse_goal_config(config).into()));
    Some(ObserverHandle {
        engine,
        goals,
        repairs,
    })
}

fn parse_observer_config(config: &ConfigService) -> Option<ObserverConfig> {
    let value = config
        .merged()
        .get("observer")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match serde_json::from_value::<ObserverConfig>(value) {
        Ok(observer) if observer.enabled => Some(observer),
        Ok(_) => {
            tracing::debug!("observer disabled by configuration");
            None
        }
        Err(error) => {
            tracing::warn!(error = %error, "observer disabled: invalid observer configuration");
            None
        }
    }
}

fn parse_goal_config(config: &ConfigService) -> GoalConfig {
    config
        .merged()
        .get("goal")
        .and_then(|value| serde_json::from_value::<GoalConfig>(value.clone()).ok())
        .unwrap_or_default()
}

fn resolve_model(
    config: &ConfigService,
    catalog: &Catalog,
    observer: &ObserverConfig,
) -> Option<ResolvedModel> {
    let reference = observer
        .model
        .clone()
        .or_else(|| observer.digest_model.clone())
        .or_else(|| {
            config
                .merged()
                .get("subagents")
                .and_then(|subagents| subagents.get("digest_model"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let Some(reference) = reference else {
        tracing::info!("observer disabled: no model");
        return None;
    };
    let model = match catalog.resolve(&reference) {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(
                model = %reference,
                error = %error,
                "observer disabled: no model"
            );
            return None;
        }
    };
    if model.protocol == Protocol::Unsupported {
        tracing::warn!(
            model = %reference,
            "observer disabled: no model (unsupported protocol)"
        );
        return None;
    }
    // Credentials are resolved per request: the judge call reads the stored
    // credential and the LLM client falls back to the provider's environment
    // variables. A stored credential (or a missing one) is only knowable when
    // the judge call actually runs, so a missing env var alone must not
    // disable the observer; a failed call publishes `observer.unavailable`
    // instead.
    Some(model)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::path::Path;
    use std::time::Duration;

    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use oc_config::LoadOptions;
    use oc_observer::{GoalSnapshot, GoalStatus, RepairKind};
    use oc_store::{NewCredential, NewGoal, Store};
    use oc_types::{ServerInfo, ServerPaths};
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
          }
        }
      }
    }"#;

    /// Like [`CATALOG`], but the provider declares an environment credential.
    const ENV_CATALOG: &str = r#"{
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

    fn state_at(directory: &Path) -> AppState {
        AppState::new(
            ServerInfo {
                version: "0.0.0-test".to_owned(),
                pid: 4242,
                hostname: Some("127.0.0.1".to_owned()),
                urls: vec!["http://127.0.0.1:4096".to_owned()],
                paths: ServerPaths {
                    tmp: std::env::temp_dir().to_string_lossy().into_owned(),
                },
                capabilities: None,
            },
            None,
            directory.to_path_buf(),
        )
    }

    async fn store_at(directory: &Path) -> Arc<Store> {
        Arc::new(
            Store::open(directory.join("db.sqlite"))
                .await
                .expect("store opens"),
        )
    }

    async fn insert_goal(store: &Store, id: &str, root_id: &str) {
        store
            .set_goal(NewGoal {
                id: id.to_owned(),
                root_id: root_id.to_owned(),
                objective: "ship it".to_owned(),
                criteria: vec!["tests pass".to_owned()],
                model: None,
                time_created: oc_types::now_millis(),
            })
            .await
            .expect("goal inserted");
    }

    #[tokio::test]
    async fn disabled_when_observer_is_off() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"observer": {"enabled": false, "model": "test/alpha"}}"#,
        );
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
        assert!(handle.is_none());
    }

    #[tokio::test]
    async fn disabled_without_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"enabled": true}}"#);
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
        assert!(handle.is_none());
    }

    #[tokio::test]
    async fn enabled_with_explicit_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"observer": {"model": "test/alpha", "tick_ms": 250}}"#,
        );
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client)
            .expect("observer built");
        assert_eq!(handle.engine.config().model.as_deref(), Some("test/alpha"));
        assert_eq!(handle.engine.config().tick_ms, 250);
    }

    #[tokio::test]
    async fn missing_env_credential_does_not_disable_the_observer() {
        assert!(
            std::env::var_os("OC_OBSERVER_TEST_API_KEY").is_none(),
            "test requires the provider credential to stay unset"
        );
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"model": "test/alpha"}}"#);
        let catalog = Catalog::from_snapshot(ENV_CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));

        // The environment variable alone cannot decide availability: a stored
        // credential may exist, so the observer builds and fails open on the
        // first judge call when no credential is usable.
        let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
        assert!(handle.is_some());
    }

    #[tokio::test]
    async fn stored_credential_with_missing_env_builds_the_observer() {
        assert!(
            std::env::var_os("OC_OBSERVER_TEST_API_KEY").is_none(),
            "test requires the provider credential to stay unset"
        );
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"model": "test/alpha"}}"#);
        let catalog = Catalog::from_snapshot(ENV_CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        store
            .create_credential(NewCredential {
                id: Some("cred_observer".to_owned()),
                integration_id: "test".to_owned(),
                label: None,
                value: json!({"type": "key", "key": "river-key"}),
                active: Some(true),
            })
            .await
            .expect("credential");
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));

        let handle = build_for_state(&state_at(root.path()), store, &config, &catalog, client);
        assert!(handle.is_some());
    }

    #[tokio::test]
    async fn falls_back_to_observer_digest_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"observer": {"digest_model": "test/alpha"}}"#,
        );
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        assert!(
            build_for_state(&state_at(root.path()), store, &config, &catalog, client).is_some()
        );
    }

    #[tokio::test]
    async fn falls_back_to_subagents_digest_model() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(
            root.path(),
            r#"{"subagents": {"digest_model": "test/alpha"}}"#,
        );
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        assert!(
            build_for_state(&state_at(root.path()), store, &config, &catalog, client).is_some()
        );
    }

    #[tokio::test]
    async fn unknown_model_disables() {
        let root = TempDir::new().expect("tempdir");
        let config = config_at(root.path(), r#"{"observer": {"model": "test/missing"}}"#);
        let catalog = Catalog::from_snapshot(CATALOG).expect("catalog");
        let store = store_at(root.path()).await;
        let client = Arc::new(SharedLlmClient::new(
            oc_llm::LlmClient::new().expect("client"),
        ));
        assert!(
            build_for_state(&state_at(root.path()), store, &config, &catalog, client).is_none()
        );
    }

    async fn notifier_fixture() -> (
        BusObserverNotifier,
        broadcast::Receiver<Arc<EventEnvelope>>,
        Arc<Store>,
        TempDir,
    ) {
        let root = TempDir::new().expect("tempdir");
        let store = store_at(root.path()).await;
        let (tx, rx) = broadcast::channel(16);
        let notifier =
            BusObserverNotifier::new(tx, Arc::clone(&store), Arc::from("/tmp/observer-work"));
        (notifier, rx, store, root)
    }

    async fn next_event(rx: &mut broadcast::Receiver<Arc<EventEnvelope>>) -> Arc<EventEnvelope> {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event within timeout")
            .expect("event")
    }

    #[tokio::test]
    async fn repaired_persists_then_publishes() {
        let (notifier, mut rx, store, _root) = notifier_fixture().await;
        notifier.notify(ObserverNotice::Repaired {
            root_id: "ses_root".to_owned(),
            target: "ses_child".to_owned(),
            kind: RepairKind::CompactContext,
            reason: "child is looping".to_owned(),
            attempt: 1,
            budget_left: 4,
        });
        let event = next_event(&mut rx).await;
        assert_eq!(event.kind, "observer.repaired");
        assert_eq!(event.data["rootID"], "ses_root");
        assert_eq!(event.data["target"], "ses_child");
        assert_eq!(event.data["kind"], "compact_context");
        assert_eq!(event.data["attempt"], 1);
        assert_eq!(event.data["budgetLeft"], 4);
        assert_eq!(
            event
                .location
                .as_ref()
                .map(|location| location.directory.as_str()),
            Some("/tmp/observer-work")
        );
        assert!(event.durable.is_none());

        // The event is published after the audit row, so it must exist now.
        let rows = store.list_repairs("ses_root", 10).await.expect("repairs");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "compact_context");
        assert_eq!(rows[0]["targetID"], "ses_child");
        assert_eq!(rows[0]["attempt"], 1);
        assert!(
            rows[0]["argsHash"]
                .as_str()
                .is_some_and(|hash| !hash.is_empty())
        );
    }

    #[tokio::test]
    async fn unavailable_is_rate_limited() {
        let (notifier, mut rx, _store, _root) = notifier_fixture().await;
        notifier.notify(ObserverNotice::Unavailable {
            root_id: "ses_root".to_owned(),
            reason: "timeout".to_owned(),
        });
        let event = next_event(&mut rx).await;
        assert_eq!(event.kind, "observer.unavailable");
        assert_eq!(event.data["reason"], "timeout");

        notifier.notify(ObserverNotice::Unavailable {
            root_id: "ses_root".to_owned(),
            reason: "timeout again".to_owned(),
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "second unavailable notice must be rate-limited"
        );

        // A different root has its own window.
        notifier.notify(ObserverNotice::Unavailable {
            root_id: "ses_other".to_owned(),
            reason: "timeout".to_owned(),
        });
        let event = next_event(&mut rx).await;
        assert_eq!(event.kind, "observer.unavailable");
        assert_eq!(event.data["rootID"], "ses_other");
    }

    #[tokio::test]
    async fn escalated_publishes_budget() {
        let (notifier, mut rx, _store, _root) = notifier_fixture().await;
        notifier.notify(ObserverNotice::Escalated {
            root_id: "ses_root".to_owned(),
            reason: "budget exhausted".to_owned(),
            budget: oc_observer::BudgetState {
                root_used: 5,
                root_limit: 5,
                targets: Vec::new(),
                exhausted: true,
            },
        });
        let event = next_event(&mut rx).await;
        assert_eq!(event.kind, "observer.escalated");
        assert_eq!(event.data["budget"]["exhausted"], true);
        assert_eq!(event.data["budget"]["root_used"], 5);
    }

    #[tokio::test]
    async fn goal_publishes_snapshot() {
        let (notifier, mut rx, store, _root) = notifier_fixture().await;
        insert_goal(&store, "gol_1", "ses_root").await;
        let goal = GoalSnapshot {
            id: "gol_1".to_owned(),
            objective: "ship it".to_owned(),
            status: GoalStatus::Done,
            done: 1,
            total: 1,
            ..GoalSnapshot::default()
        };
        notifier.notify(ObserverNotice::Goal {
            root_id: "ses_root".to_owned(),
            goal_id: "gol_1".to_owned(),
            status: GoalStatus::Done,
            reason: "criteria met".to_owned(),
            goal,
        });
        let event = next_event(&mut rx).await;
        assert_eq!(event.kind, "goal.updated");
        assert_eq!(event.data["rootID"], "ses_root");
        assert_eq!(event.data["goalID"], "gol_1");
        assert_eq!(event.data["status"], "done");
        assert_eq!(event.data["reason"], "criteria met");
        assert_eq!(event.data["goal"]["objective"], "ship it");
        assert_eq!(
            event
                .location
                .as_ref()
                .map(|location| location.directory.as_str()),
            Some("/tmp/observer-work")
        );
    }

    #[tokio::test]
    async fn goal_transition_persists_then_publishes() {
        let (notifier, mut rx, store, _root) = notifier_fixture().await;
        insert_goal(&store, "gol_1", "ses_root").await;

        let goal = GoalSnapshot {
            id: "gol_1".to_owned(),
            objective: "ship it".to_owned(),
            criteria: vec!["tests pass".to_owned()],
            done: 1,
            total: 1,
            status: GoalStatus::Done,
            rounds: 4,
            stalls: 2,
            evidence: vec!["cal_1".to_owned()],
            last_progress_ms: 1_700_000_000_000,
            ..GoalSnapshot::default()
        };
        notifier.notify(ObserverNotice::Goal {
            root_id: "ses_root".to_owned(),
            goal_id: "gol_1".to_owned(),
            status: GoalStatus::Done,
            reason: "criteria met".to_owned(),
            goal,
        });

        // The event is published after the update, so the row must already
        // carry the transitioned state when the event arrives.
        let event = next_event(&mut rx).await;
        assert_eq!(event.kind, "goal.updated");
        assert_eq!(event.data["goalID"], "gol_1");
        assert_eq!(event.data["status"], "done");
        assert_eq!(event.data["goal"]["rounds"], 4);
        assert_eq!(event.data["goal"]["stalls"], 2);

        let stored = store
            .get_goal("ses_root")
            .await
            .expect("goal read")
            .expect("goal row");
        assert_eq!(stored["id"], "gol_1");
        assert_eq!(stored["status"], "done");
        assert_eq!(stored["rounds"], 4);
        assert_eq!(stored["stalls"], 2);
        assert_eq!(stored["lastProgress"], 1_700_000_000_000_i64);
        assert_eq!(stored["evidence"], json!(["cal_1"]));

        assert!(
            tokio::time::timeout(Duration::from_millis(150), rx.recv())
                .await
                .is_err(),
            "exactly one goal.updated event per transition"
        );
    }

    #[tokio::test]
    async fn goal_transition_without_row_publishes_nothing() {
        let (notifier, mut rx, _store, _root) = notifier_fixture().await;
        let goal = GoalSnapshot {
            id: "gol_missing".to_owned(),
            objective: "ship it".to_owned(),
            status: GoalStatus::Done,
            done: 1,
            total: 1,
            rounds: 3,
            ..GoalSnapshot::default()
        };
        notifier.notify(ObserverNotice::Goal {
            root_id: "ses_root".to_owned(),
            goal_id: "gol_missing".to_owned(),
            status: GoalStatus::Done,
            reason: "criteria met".to_owned(),
            goal,
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(250), rx.recv())
                .await
                .is_err(),
            "a missing goal row must not publish goal.updated"
        );
    }

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
}
