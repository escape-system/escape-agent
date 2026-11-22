//! Model and provider routes.
//!
//! All four routes use the location envelope (`{location, data}`). There is no
//! fallback catalog: when none is attached the routes answer the documented
//! `503 ServiceUnavailableError` instead of inventing an empty model list.
//!
//! The catalog is immutable once attached; the `providers` configuration
//! overlay is applied when the catalog is built at startup, not per request.
//! See [`crate::catalog_from_config`].
//!
//! Only available providers are exposed: explicitly configured providers,
//! providers with a stored credential, and providers whose environment variable
//! is set. Disabled providers are never available.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use oc_antigravity::ANTIGRAVITY_PROVIDER_ID;
use oc_llm::catalog::refresh;
use oc_llm::{Activation, Catalog, LlmError, ModelInfo, ProviderInfo};
use oc_types::{ApiError, DataEnvelope, EventEnvelope, LocationEnvelope};
use serde_json::{Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::require_catalog;
use crate::data::require_config;
use crate::error::ErrorResponse;

/// Parsed query pairs. Repeated keys (`?types=a&types=b`) are preserved.
struct Params {
    pairs: Vec<(String, String)>,
}

impl Params {
    fn parse(raw: Option<&str>) -> Self {
        let pairs = url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
            .into_owned()
            .collect();
        Self { pairs }
    }

    fn first(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Values of `key`, split on commas and trimmed; empty values dropped.
    fn list(&self, key: &str) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(name, _)| name == key)
            .flat_map(|(_, value)| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// Providers the client may actually use: configured in the `providers` map,
/// backed by a stored credential, or reachable through one of their environment
/// variables. Disabled providers are never available.
///
/// The credential store is optional: without one only configuration and
/// environment variables decide availability.
pub(crate) async fn available_provider_ids(
    state: &AppState,
    catalog: &Catalog,
) -> BTreeSet<String> {
    fn add(catalog: &Catalog, available: &mut BTreeSet<String>, id: &str) {
        if catalog
            .provider(id)
            .is_some_and(|provider| provider.activation != Activation::Disabled)
        {
            available.insert(id.to_owned());
        }
    }

    let mut available = BTreeSet::new();

    if let Some(config) = state.config()
        && let Some(providers) = config.merged().get("providers").and_then(Value::as_object)
    {
        // A configured provider is available even without a credential: the
        // `settings.apiKey` / `settings.baseURL` overlay belongs to these keys.
        // Snapshot providers also carry a `baseURL`, so settings must not be
        // treated as an availability signal on their own.
        for id in providers.keys() {
            add(catalog, &mut available, id);
        }
    }

    if let Some(store) = state.store() {
        match store.list_credentials().await {
            Ok(entries) => {
                for entry in entries {
                    if let Some(id) = entry.get("integrationID").and_then(Value::as_str) {
                        add(catalog, &mut available, id);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not list credentials for provider availability");
            }
        }
    }

    for provider in catalog.providers() {
        if provider.activation == Activation::Disabled {
            continue;
        }
        if provider
            .env
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            available.insert(provider.id.clone());
        }
    }

    if let Some(ag) = state.antigravity()
        && ag.account_count() > 0
    {
        add(catalog, &mut available, "google");
    }

    available
}

/// Providers the quota report may surface: only those with a **genuinely
/// active** credential (or a usable Antigravity account), never providers that
/// merely exist in the catalog, in history, or as a disabled credential row.
///
/// This is deliberately separate from [`available_provider_ids`]: the model and
/// provider routes must keep listing a provider whose credential the user
/// deactivated (the settings UI has to offer re-activation), whereas the quota
/// report measures live usage and must not show a card for it.
///
/// Rules, in order:
///
/// 1. An **active** credential row (`active == true`). `opencode-go` also
///    accepts an active `opencode` credential because it is an alias of that
///    integration. A `google` OAuth row is Antigravity-managed and is decided by
///    rule 4 instead; only a direct (non-OAuth) `google` key counts here.
/// 2. `providers` configuration keys and environment variables, but **only**
///    for provider ids that own no credential row at all. That keeps a plain
///    `ANTHROPIC_API_KEY` / `providers.anthropic` setup working while a
///    provider whose stored credential was deactivated stays hidden.
/// 3. `google` only when Antigravity has at least one usable account
///    (`enabled && !revoked && !verification_required`).
pub(crate) async fn active_provider_ids(state: &AppState, catalog: &Catalog) -> BTreeSet<String> {
    fn add(catalog: &Catalog, available: &mut BTreeSet<String>, id: &str) {
        if catalog
            .provider(id)
            .is_some_and(|provider| provider.activation != Activation::Disabled)
        {
            available.insert(id.to_owned());
        }
    }

    let mut active = BTreeSet::new();
    let antigravity_attached = state.antigravity().is_some();

    // Integration ids that own at least one credential row, active or not. An
    // env/config-only provider must own no row at all (rule 2).
    let mut credential_integrations: BTreeSet<String> = BTreeSet::new();
    // Integrations with at least one `active == true` row. Kept apart from
    // `active` so the `opencode-go` alias works even when `opencode` itself is
    // absent from the catalog.
    let mut active_integrations: BTreeSet<String> = BTreeSet::new();
    // `google` reached through a direct API key rather than the account pool.
    let mut google_direct_key = false;
    if let Some(store) = state.store() {
        match store.list_credentials().await {
            Ok(entries) => {
                for entry in entries {
                    let Some(id) = entry.get("integrationID").and_then(Value::as_str) else {
                        continue;
                    };
                    credential_integrations.insert(id.to_owned());
                    if entry.get("active").and_then(Value::as_bool) != Some(true) {
                        continue;
                    }
                    active_integrations.insert(id.to_owned());
                    let is_oauth =
                        entry.pointer("/value/type").and_then(Value::as_str) == Some("oauth");
                    if id == ANTIGRAVITY_PROVIDER_ID && antigravity_attached {
                        // The pool owns an OAuth `google` credential: a disabled
                        // or revoked account must not surface a quota card.
                        if !is_oauth {
                            google_direct_key = true;
                        }
                        continue;
                    }
                    add(catalog, &mut active, id);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not list credentials for active provider availability");
            }
        }
    }

    // `opencode-go` is an alias of the `opencode` integration: an active
    // `opencode` credential is a valid credential for `opencode-go` too, and no
    // other credential family may stand in.
    if active_integrations.contains("opencode") {
        add(catalog, &mut active, "opencode-go");
    }
    if google_direct_key {
        add(catalog, &mut active, ANTIGRAVITY_PROVIDER_ID);
    }

    // Rule 2: configuration and environment only decide availability for
    // providers that store no credential at all.
    if let Some(config) = state.config()
        && let Some(providers) = config.merged().get("providers").and_then(Value::as_object)
    {
        for id in providers.keys() {
            if credential_integrations.contains(id) {
                continue;
            }
            add(catalog, &mut active, id);
        }
    }

    for provider in catalog.providers() {
        if provider.activation == Activation::Disabled {
            continue;
        }
        if credential_integrations.contains(&provider.id) {
            continue;
        }
        if provider
            .env
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            active.insert(provider.id.clone());
        }
    }

    if antigravity_attached
        && let Some(ag) = state.antigravity()
        && ag.has_usable_accounts()
    {
        add(catalog, &mut active, ANTIGRAVITY_PROVIDER_ID);
    }

    active
}

/// True when `"google"` is configured via a direct (non-OAuth) credential,
/// `providers.google` config entry, or a Google API key environment variable.
/// When false and Antigravity is attached, `"google"` is served exclusively by
/// Antigravity OAuth, so raw `models.dev` AI Studio entries are omitted.
pub(crate) async fn has_direct_google_access(state: &AppState, catalog: &Catalog) -> bool {
    if state.antigravity().is_none() {
        return true;
    }
    if let Some(config) = state.config()
        && config
            .merged()
            .get("providers")
            .and_then(Value::as_object)
            .is_some_and(|p| p.contains_key(ANTIGRAVITY_PROVIDER_ID))
    {
        return true;
    }
    if let Some(store) = state.store()
        && let Ok(entries) = store.list_credentials().await
        && entries.iter().any(|entry| {
            entry.get("integrationID").and_then(Value::as_str) == Some(ANTIGRAVITY_PROVIDER_ID)
                && entry.get("active").and_then(Value::as_bool) == Some(true)
                && entry.pointer("/value/type").and_then(Value::as_str) != Some("oauth")
        })
    {
        return true;
    }
    catalog
        .provider(ANTIGRAVITY_PROVIDER_ID)
        .is_some_and(|provider| {
            provider
                .env
                .iter()
                .any(|name| std::env::var_os(name).is_some())
        })
}

/// `GET /api/model?types=`
pub async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<LocationEnvelope<Vec<ModelInfo>>>, ErrorResponse> {
    let params = Params::parse(raw.as_deref());
    let catalog = require_catalog(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.first("location[directory]"));

    let available = available_provider_ids(&state, catalog.as_ref()).await;
    let direct_google = has_direct_google_access(&state, catalog.as_ref()).await;
    let types = params.list("types");
    let mut models = catalog.models();
    models.retain(|model| available.contains(&model.provider_id));
    if available.contains("google")
        && let Some(ag) = state.antigravity()
    {
        if !direct_google {
            models.retain(|m| m.provider_id != "google");
        }
        for extra in ag.antigravity_model_infos() {
            if let Some(pos) = models
                .iter()
                .position(|m| m.provider_id == "google" && m.id == extra.id)
            {
                models[pos] = extra;
            } else {
                models.push(extra);
            }
        }
    }
    if !types.is_empty() {
        models.retain(|model| {
            model
                .kind
                .as_deref()
                .is_some_and(|kind| types.iter().any(|wanted| wanted.eq_ignore_ascii_case(kind)))
        });
    }

    Ok(Json(LocationEnvelope::new(resolved.reference(), models)))
}

/// `POST /api/model/refresh`
///
/// Fetch the published catalog on demand, replace the served generation under
/// the same plausibility gate and overlays as the background loop, and announce
/// the swap. Failures are reported honestly: no event and no cell swap happens
/// until the whole body has been accepted.
pub async fn refresh_models(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Some(cell) = state.catalog_cell().cloned() else {
        return Err(ApiError::service_unavailable("model catalog not loaded").into());
    };
    let url = refresh::models_url();
    let cache = refresh::cache_path();
    let client = refresh::client();
    let (opencode_go_result, nous_result) = tokio::join!(
        refresh::fetch_opencode_go_models_now(&client),
        refresh::fetch_nous_models_now(&client),
    );
    let opencode_go_models = match opencode_go_result {
        Ok(models) => Some(models),
        Err(error) => {
            tracing::debug!(%error, "live opencode-go model list refresh skipped");
            None
        }
    };
    let nous_models = match nous_result {
        Ok((raw, models)) => {
            if let Err(error) = refresh::write_cache(&refresh::nous_cache_path(), &raw) {
                tracing::debug!(%error, "nous model list cache write skipped");
            }
            Some(models)
        }
        Err(error) => {
            tracing::debug!(%error, "live nous model list refresh skipped");
            None
        }
    };
    // AtomicUsize keeps the captured counter Send so this handler can be routed.
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0_usize));
    let apply = |body: &str| -> Result<(), String> {
        let mut live = refresh::parse_live(body).map_err(|e| e.to_string())?;
        if !refresh::is_plausible(&live) {
            return Err(format!(
                "published catalog has only {} models",
                live.models().len()
            ));
        }
        refresh::apply_builtin_providers(&mut live);
        if let Some(models) = &opencode_go_models {
            live.upsert_provider_models(models.clone());
        }
        if let Some(models) = &nous_models {
            live.upsert_provider_models(models.clone());
        } else if let Some(Ok(cached_body)) = refresh::read_cache(&refresh::nous_cache_path())
            && let Ok(cached_models) = refresh::parse_nous_models(&cached_body)
        {
            live.upsert_provider_models(cached_models);
        }
        if let Some(config) = state.config()
            && let Some(providers) = config.merged().get("providers")
        {
            live.apply_config(providers).map_err(|e| e.to_string())?;
        }
        if let Some(antigravity) = state.antigravity() {
            antigravity.overlay_catalog(&mut live);
        }
        count.store(live.models().len(), std::sync::atomic::Ordering::SeqCst);
        cell.replace(live);
        state.publish(EventEnvelope::now("models-dev.refreshed", json!({})));
        Ok(())
    };
    refresh::refresh_now(&client, &url, &cache, apply)
        .await
        .map_err(|error| ApiError::new(502, "ProviderError", error))?;
    Ok(Json(DataEnvelope::new(json!({
        "modelCount": count.load(std::sync::atomic::Ordering::SeqCst),
        "refreshed": true,
    }))))
}

/// `GET /api/model/default`
pub async fn default_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<LocationEnvelope<ModelInfo>>, ErrorResponse> {
    let params = Params::parse(raw.as_deref());
    let catalog = require_catalog(&state)?;
    let config = require_config(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.first("location[directory]"));

    let Some(reference) = config.model() else {
        return Err(model_not_found("", "", "No default model configured".to_owned()).into());
    };
    // `resolve` reports unknown providers/models, invalid references and
    // unknown variants; all of them are a missing default model to the client.
    let model = match catalog.resolve(&reference) {
        Ok(model) => model,
        Err(error) => {
            if let Some(ag) = state.antigravity()
                && let Some(resolved) = ag.resolve_catalog_model(catalog.as_ref(), &reference)
            {
                resolved
            } else {
                return Err(ErrorResponse::from(resolve_error(&error)));
            }
        }
    };
    let available = available_provider_ids(&state, catalog.as_ref()).await;
    if !available.contains(&model.provider.id) {
        return Err(model_not_found(
            &model.provider.id,
            &model.model.id,
            format!(
                "Default model provider is not available: {}",
                model.provider.id
            ),
        )
        .into());
    }

    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        model.model,
    )))
}

/// `GET /api/provider`
pub async fn providers(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<LocationEnvelope<Vec<ProviderInfo>>>, ErrorResponse> {
    let params = Params::parse(raw.as_deref());
    let catalog = require_catalog(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.first("location[directory]"));
    let available = available_provider_ids(&state, catalog.as_ref()).await;
    let mut providers = catalog.providers();
    providers.retain(|provider| available.contains(&provider.id));
    Ok(Json(LocationEnvelope::new(resolved.reference(), providers)))
}

/// `GET /api/provider/:id`
pub async fn provider(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Json<LocationEnvelope<ProviderInfo>>, ErrorResponse> {
    let params = Params::parse(raw.as_deref());
    let catalog = require_catalog(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.first("location[directory]"));
    let available = available_provider_ids(&state, catalog.as_ref()).await;
    let provider = catalog.provider(&id).filter(|_| available.contains(&id));
    let Some(provider) = provider else {
        return Err(ApiError::not_found(
            "ProviderNotFoundError",
            format!("Provider not found: {id}"),
        )
        .with("providerID", json!(id))
        .into());
    };
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        provider.clone(),
    )))
}

fn model_not_found(provider: &str, model: &str, message: String) -> ApiError {
    ApiError::not_found("ModelNotFoundError", message)
        .with("providerID", json!(provider))
        .with("modelID", json!(model))
}

/// Turn a catalog resolution failure into the wire `ModelNotFoundError`.
fn resolve_error(error: &LlmError) -> ApiError {
    match error {
        LlmError::UnknownModel {
            provider, model, ..
        }
        | LlmError::UnknownVariant {
            provider, model, ..
        }
        | LlmError::UnsupportedProtocol {
            provider, model, ..
        } => model_not_found(provider, model, error.to_string()),
        LlmError::UnknownProvider { provider, .. } => {
            model_not_found(provider, "", error.to_string())
        }
        _ => model_not_found("", "", error.to_string()),
    }
}
