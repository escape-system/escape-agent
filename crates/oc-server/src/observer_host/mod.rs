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

mod model;
mod notifier;

use std::fmt;
use std::sync::Arc;

use oc_config::ConfigService;
use oc_llm::{Catalog, Protocol, ResolvedModel, SharedLlmClient};
use oc_observer::{GoalConfig, GoalStore, ObserverConfig, ObserverEngine, SystemClock};
use oc_runtime::{RepairChannel, RepairCommand, StoreGoalStore};
use oc_store::Store;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::AppState;

pub use model::LlmObserverModel;
pub use notifier::BusObserverNotifier;

/// Maximum tokens the observer may spend on one verdict.
const VERDICT_MAX_TOKENS: u64 = 4096;

/// Minimum gap between two `observer.unavailable` events per root.
const UNAVAILABLE_NOTICE_INTERVAL_MS: i64 = 300_000;

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
mod tests;
