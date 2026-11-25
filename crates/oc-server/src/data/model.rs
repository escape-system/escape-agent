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
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use oc_antigravity::ANTIGRAVITY_PROVIDER_ID;
use oc_llm::catalog::refresh;
use oc_llm::{Activation, Catalog, LlmError, ModelInfo, ProviderInfo};
use oc_types::{ApiError, DataEnvelope, EventEnvelope, LocationEnvelope, LocationRef};
use serde_json::{Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::require_catalog;
use crate::data::require_config;
use crate::error::ErrorResponse;

#[derive(Debug, Clone)]
struct EnvProviderEntry {
    id: String,
    activation: Activation,
    env: Vec<String>,
}

#[derive(Debug, Clone)]
struct CachedModelsData {
    catalog_ptr: usize,
    available: BTreeSet<String>,
    direct_google: bool,
    ag_models: Vec<ModelInfo>,
    models: Arc<Vec<ModelInfo>>,
    json_array: Arc<Bytes>,
}

#[derive(Debug, Clone)]
struct CachedProvidersData {
    catalog_ptr: usize,
    available: BTreeSet<String>,
    json_array: Arc<Bytes>,
}

/// Per-`AppState` cache for extracted catalog provider metadata and pre-filtered
/// `/api/model` and `/api/provider` JSON payloads.
#[derive(Debug, Default)]
pub(crate) struct ModelCache {
    env_providers: Mutex<Option<(usize, Vec<EnvProviderEntry>)>>,
    models_data: Mutex<Option<CachedModelsData>>,
    providers_data: Mutex<Option<CachedProvidersData>>,
}

impl ModelCache {
    fn provider_env_list(&self, catalog: &Catalog) -> Vec<EnvProviderEntry> {
        let ptr = std::ptr::from_ref(catalog) as usize;
        {
            let guard = self
                .env_providers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((cached_ptr, entries)) = &*guard
                && *cached_ptr == ptr
            {
                return entries.clone();
            }
        }
        let entries: Vec<EnvProviderEntry> = catalog
            .providers()
            .into_iter()
            .map(|provider| EnvProviderEntry {
                id: provider.id,
                activation: provider.activation,
                env: provider.env,
            })
            .collect();
        let mut guard = self
            .env_providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some((ptr, entries.clone()));
        entries
    }
}

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

pub(crate) async fn load_credentials(state: &AppState) -> Vec<Value> {
    let Some(store) = state.store() else {
        return Vec::new();
    };
    match store.list_credentials().await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "could not list credentials for provider availability");
            Vec::new()
        }
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
    let credentials = load_credentials(state).await;
    available_provider_ids_with_credentials(state, catalog, &credentials)
}

pub(crate) fn available_provider_ids_with_credentials(
    state: &AppState,
    catalog: &Catalog,
    credentials: &[Value],
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

    for entry in credentials {
        if let Some(id) = entry.get("integrationID").and_then(Value::as_str) {
            add(catalog, &mut available, id);
        }
    }

    for provider in state.model_cache().provider_env_list(catalog) {
        if provider.activation == Activation::Disabled {
            continue;
        }
        if provider
            .env
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            available.insert(provider.id);
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
#[allow(dead_code)]
pub(crate) async fn active_provider_ids(state: &AppState, catalog: &Catalog) -> BTreeSet<String> {
    let credentials = load_credentials(state).await;
    active_provider_ids_with_credentials(state, catalog, &credentials)
}

pub(crate) fn active_provider_ids_with_credentials(
    state: &AppState,
    catalog: &Catalog,
    credentials: &[Value],
) -> BTreeSet<String> {
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

    let mut credential_integrations: BTreeSet<String> = BTreeSet::new();
    let mut active_integrations: BTreeSet<String> = BTreeSet::new();
    let mut google_direct_key = false;
    for entry in credentials {
        let Some(id) = entry.get("integrationID").and_then(Value::as_str) else {
            continue;
        };
        credential_integrations.insert(id.to_owned());
        if entry.get("active").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        active_integrations.insert(id.to_owned());
        let is_oauth = entry.pointer("/value/type").and_then(Value::as_str) == Some("oauth");
        if id == ANTIGRAVITY_PROVIDER_ID && antigravity_attached {
            if !is_oauth {
                google_direct_key = true;
            }
            continue;
        }
        add(catalog, &mut active, id);
    }

    if active_integrations.contains("opencode") {
        add(catalog, &mut active, "opencode-go");
    }
    if google_direct_key {
        add(catalog, &mut active, ANTIGRAVITY_PROVIDER_ID);
    }

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

    for provider in state.model_cache().provider_env_list(catalog) {
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
            active.insert(provider.id);
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
pub(crate) fn has_direct_google_access_with_credentials(
    state: &AppState,
    catalog: &Catalog,
    credentials: &[Value],
) -> bool {
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
    if credentials.iter().any(|entry| {
        entry.get("integrationID").and_then(Value::as_str) == Some(ANTIGRAVITY_PROVIDER_ID)
            && entry.get("active").and_then(Value::as_bool) == Some(true)
            && entry.pointer("/value/type").and_then(Value::as_str) != Some("oauth")
    }) {
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

pub(crate) fn filtered_catalog_models(
    state: &AppState,
    catalog: &Catalog,
    available: &BTreeSet<String>,
    direct_google: bool,
) -> (Arc<Vec<ModelInfo>>, Arc<Bytes>) {
    let catalog_ptr = std::ptr::from_ref(catalog) as usize;
    let ag_models = if available.contains("google")
        && let Some(ag) = state.antigravity()
    {
        ag.antigravity_model_infos()
    } else {
        Vec::new()
    };

    {
        let guard = state
            .model_cache()
            .models_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = &*guard
            && cached.catalog_ptr == catalog_ptr
            && cached.direct_google == direct_google
            && &cached.available == available
            && cached.ag_models == ag_models
        {
            return (Arc::clone(&cached.models), Arc::clone(&cached.json_array));
        }
    }

    let mut models = catalog.models();
    models.retain(|model| available.contains(&model.provider_id));
    if available.contains("google") && state.antigravity().is_some() {
        if !direct_google {
            models.retain(|m| m.provider_id != "google");
        }
        for extra in &ag_models {
            if let Some(pos) = models
                .iter()
                .position(|m| m.provider_id == "google" && m.id == extra.id)
            {
                models[pos] = extra.clone();
            } else {
                models.push(extra.clone());
            }
        }
    }
    let json_bytes = Bytes::from(serde_json::to_vec(&models).unwrap_or_else(|_| b"[]".to_vec()));
    let models = Arc::new(models);
    let json_array = Arc::new(json_bytes);
    let mut guard = state
        .model_cache()
        .models_data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(CachedModelsData {
        catalog_ptr,
        available: available.clone(),
        direct_google,
        ag_models,
        models: Arc::clone(&models),
        json_array: Arc::clone(&json_array),
    });
    (models, json_array)
}

fn filtered_providers_json(
    state: &AppState,
    catalog: &Catalog,
    available: &BTreeSet<String>,
) -> Arc<Bytes> {
    let catalog_ptr = std::ptr::from_ref(catalog) as usize;
    {
        let guard = state
            .model_cache()
            .providers_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cached) = &*guard
            && cached.catalog_ptr == catalog_ptr
            && &cached.available == available
        {
            return Arc::clone(&cached.json_array);
        }
    }
    let mut providers = catalog.providers();
    providers.retain(|provider| available.contains(&provider.id));
    let json_array = Arc::new(Bytes::from(
        serde_json::to_vec(&providers).unwrap_or_else(|_| b"[]".to_vec()),
    ));
    let mut guard = state
        .model_cache()
        .providers_data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(CachedProvidersData {
        catalog_ptr,
        available: available.clone(),
        json_array: Arc::clone(&json_array),
    });
    json_array
}

fn location_envelope_response(location: &LocationRef, data_json: &[u8]) -> Response {
    let loc_bytes = serde_json::to_vec(location).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::with_capacity(24 + loc_bytes.len() + data_json.len());
    out.extend_from_slice(br#"{"location":"#);
    out.extend_from_slice(&loc_bytes);
    out.extend_from_slice(br#","data":"#);
    out.extend_from_slice(data_json);
    out.push(b'}');
    let mut response = (StatusCode::OK, Body::from(out)).into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// `GET /api/model?types=`
pub async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Result<Response, ErrorResponse> {
    let params = Params::parse(raw.as_deref());
    let catalog = require_catalog(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.first("location[directory]"));

    let credentials = load_credentials(&state).await;
    let available = available_provider_ids_with_credentials(&state, catalog.as_ref(), &credentials);
    let direct_google =
        has_direct_google_access_with_credentials(&state, catalog.as_ref(), &credentials);
    let types = params.list("types");
    let (models, json_array) =
        filtered_catalog_models(&state, catalog.as_ref(), &available, direct_google);

    if types.is_empty() {
        return Ok(location_envelope_response(
            &resolved.reference(),
            &json_array,
        ));
    }

    let filtered: Vec<ModelInfo> = models
        .iter()
        .filter(|model| {
            model
                .kind
                .as_deref()
                .is_some_and(|kind| types.iter().any(|wanted| wanted.eq_ignore_ascii_case(kind)))
        })
        .cloned()
        .collect();
    Ok(Json(LocationEnvelope::new(resolved.reference(), filtered)).into_response())
}

/// `POST /api/model/refresh`
pub async fn refresh_models(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Some(cell) = state.catalog_cell().cloned() else {
        return Err(ApiError::service_unavailable("model catalog not loaded").into());
    };
    let url = refresh::models_url();
    let cache = refresh::cache_path();
    let client = refresh::client();
    let go_url = refresh::opencode_go_models_url();
    let (opencode_go_result, nous_result) = tokio::join!(
        refresh::fetch_opencode_go_models_with_body(&client, &go_url),
        refresh::fetch_nous_models_now(&client),
    );
    let opencode_go = match opencode_go_result {
        Ok((raw, models)) => {
            if let Err(error) = refresh::write_cache(&refresh::opencode_go_cache_path(), &raw) {
                tracing::debug!(%error, "opencode-go model list cache write skipped");
            }
            Some(models)
        }
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
        if let Some(models) = &opencode_go {
            let (upserted, removed) = refresh::apply_provider_models(
                &mut live,
                refresh::OPENCODE_GO_PROVIDER_ID,
                models.clone(),
            );
            tracing::debug!(upserted, removed, "opencode-go membership applied");
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
) -> Result<Response, ErrorResponse> {
    let params = Params::parse(raw.as_deref());
    let catalog = require_catalog(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.first("location[directory]"));
    let available = available_provider_ids(&state, catalog.as_ref()).await;
    let json_array = filtered_providers_json(&state, catalog.as_ref(), &available);
    Ok(location_envelope_response(
        &resolved.reference(),
        &json_array,
    ))
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
