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
use oc_llm::{Activation, Catalog, LlmError, ModelInfo, ProviderInfo};
use oc_types::{ApiError, LocationEnvelope};
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
async fn available_provider_ids(state: &AppState, catalog: &Catalog) -> BTreeSet<String> {
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

    available
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
    let types = params.list("types");
    let mut models = catalog.models();
    models.retain(|model| available.contains(&model.provider_id));
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
    let model = catalog
        .resolve(&reference)
        .map_err(|error| ErrorResponse::from(resolve_error(&error)))?;
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
