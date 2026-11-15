//! Unified multi-provider quota, balance, rate-limit, and local usage routes.
//!
//! - `GET /api/quotas` (`?refresh=true|false`)
//! - `POST /api/quotas/refresh`
//! - `PUT /api/quotas/openrouter/management-key`
//! - `DELETE /api/quotas/openrouter/management-key`
//!
//! This module is the facade: it owns the cache handle, the router, the wire
//! bodies and the handlers; probing, credential resolution and the local usage
//! scan live in the sibling submodules.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::{get, post, put};
use oc_llm::ProviderQuotaProbeResult;
use oc_types::{ApiError, DataEnvelope, now_millis};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::AppState;
use crate::data::{no_content, query_error, require_store, schema_error};
use crate::error::ErrorResponse;

mod aggregate;
mod antigravity;
mod credentials;
mod openrouter;
mod providers;
mod types;
mod usage;

use aggregate::build_unified_quotas;
use openrouter::{
    clear_openrouter_management_key, env_managed_error, is_env_managed,
    save_openrouter_management_key,
};
use usage::UsageCache;

/// Synthetic credential integration ID used when only `OPENROUTER_API_KEY` env var
/// is present and the user saves an `OpenRouter` Management Key via `/api/quotas`.
pub(crate) const OPENROUTER_MGMT_INTEGRATION_ID: &str = "openrouter-management";

pub(super) const CACHE_TTL_MS: i64 = 60_000;

/// Cached live provider probes and local usage rollup held in [`AppState`].
#[derive(Debug, Default)]
pub struct QuotaCache {
    pub(in crate::data::quotas) probes: BTreeMap<String, ProviderQuotaProbeResult>,
    pub(in crate::data::quotas) usage: UsageCache,
    /// When the last full probe pass finished (epoch ms); `0` means the cache
    /// has never been filled. Only a full pass writes it.
    pub(in crate::data::quotas) computed_at_ms: i64,
    /// Single-flight guard for the background refresh a GET starts.
    pub(in crate::data::quotas) refreshing: bool,
}

impl QuotaCache {
    /// Whether the snapshot is young enough to serve without a refresh.
    #[must_use]
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        self.computed_at_ms > 0 && now_ms.saturating_sub(self.computed_at_ms) < CACHE_TTL_MS
    }

    /// Whether a background refresh is running right now.
    #[must_use]
    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    /// Claim the single background refresh slot; `false` means one is running
    /// already and the caller must join it instead of starting another pass.
    pub fn begin_refresh(&mut self) -> bool {
        if self.refreshing {
            return false;
        }
        self.refreshing = true;
        true
    }

    /// Release the background refresh slot.
    pub fn end_refresh(&mut self) {
        self.refreshing = false;
    }

    /// Remove a cached provider entry so the next read re-probes upstream.
    pub fn invalidate(&mut self, provider_id: &str) {
        self.probes.remove(provider_id);
        self.computed_at_ms = 0;
    }

    /// Drop every cached probe and the usage rollup so the next read
    /// recomputes both halves.
    pub fn invalidate_all(&mut self) {
        self.probes.clear();
        self.usage = UsageCache::default();
        self.computed_at_ms = 0;
    }
}

/// Protected quota/usage control-plane routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/quotas", get(get_quotas))
        .route("/api/quotas/refresh", post(refresh_quotas))
        .route(
            "/api/quotas/openrouter/management-key",
            put(set_openrouter_management_key).delete(delete_openrouter_management_key),
        )
}

#[derive(Debug, Default, Deserialize)]
pub struct QuotasQuery {
    #[serde(default)]
    refresh: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementKeyBody {
    key: String,
}

/// `GET /api/quotas?refresh=true|false`
///
/// Cache-first: this path never awaits a provider probe. A missing or stale
/// snapshot is returned as it stands — a cold cache answers HTTP 200 with an
/// empty provider list and `refreshing: true` — and exactly one background
/// refresh is started behind it. `?refresh=true` keeps the historical blocking
/// behavior for callers that cannot poll.
pub async fn get_quotas(
    State(state): State<AppState>,
    params: Result<Query<QuotasQuery>, QueryRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let blocking = params.refresh.unwrap_or(false);
    let mut payload = build_unified_quotas(&state, blocking, blocking).await?;
    if !blocking {
        payload.refreshing = start_background_refresh(&state).await;
    }
    let value = serde_json::to_value(payload).map_err(|err| ApiError::internal(err.to_string()))?;
    Ok(Json(DataEnvelope::new(value)))
}

/// Start one background refresh when the cached snapshot is missing or older
/// than [`CACHE_TTL_MS`], and report whether the payload being served may be
/// stale — which is exactly what the wire `refreshing` flag means.
///
/// Single-flight: the [`QuotaCache::begin_refresh`] claim makes a second
/// request join the in-flight run instead of starting another probe pass.
async fn start_background_refresh(state: &AppState) -> bool {
    let now_ms = now_millis();
    let (is_stale, in_flight) = {
        let cache = state.quota_cache().read().await;
        (!cache.is_fresh(now_ms), cache.is_refreshing())
    };
    if in_flight {
        return true;
    }
    if !is_stale {
        return false;
    }
    if !state.quota_cache().write().await.begin_refresh() {
        return true;
    }
    let background = state.clone();
    tokio::spawn(async move {
        // A full pass: the cache is stale by definition here, so both the
        // provider probes and the local-usage rollup are recomputed.
        if build_unified_quotas(&background, true, true).await.is_err() {
            tracing::warn!("background quota refresh failed");
        }
        background.quota_cache().write().await.end_refresh();
    });
    true
}

/// `POST /api/quotas/refresh`
pub async fn refresh_quotas(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    state.quota_cache().write().await.invalidate_all();
    let payload = build_unified_quotas(&state, true, true).await?;
    let value = serde_json::to_value(payload).map_err(|err| ApiError::internal(err.to_string()))?;
    Ok(Json(DataEnvelope::new(value)))
}

/// `PUT /api/quotas/openrouter/management-key`
pub async fn set_openrouter_management_key(
    State(state): State<AppState>,
    body: Result<Json<ManagementKeyBody>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let key = body.key.trim();
    if key.is_empty() {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "Management key cannot be empty",
        )));
    }
    let store = require_store(&state)?;
    save_openrouter_management_key(store, key).await?;
    state.quota_cache().write().await.invalidate("openrouter");
    Ok(Json(DataEnvelope::new(json!({ "configured": true }))))
}

/// `DELETE /api/quotas/openrouter/management-key`
pub async fn delete_openrouter_management_key(
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    let credentials = store.list_credentials().await?;
    if is_env_managed(&credentials) {
        return Err(ErrorResponse::from(env_managed_error()));
    }
    clear_openrouter_management_key(store).await?;
    state.quota_cache().write().await.invalidate("openrouter");
    Ok(no_content())
}

/// Shared cache handle helper used by [`AppState`].
pub(crate) fn new_quota_cache() -> Arc<RwLock<QuotaCache>> {
    Arc::new(RwLock::new(QuotaCache::default()))
}
