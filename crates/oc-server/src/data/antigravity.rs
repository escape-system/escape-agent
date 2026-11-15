//! `/api/antigravity` control-plane routes for Google Antigravity multi-account
//! pool status, selection, enable/disable, removal, live quota inspection, and
//! dynamic model discovery refresh.
//!
//! `GET /api/antigravity/quotas` serves the cached pool snapshot. It accepts
//! `?refresh=true` to bypass the cache and run a live `refresh_quotas_and_models`
//! probe (reported truthfully under `report`), and `?id=<index|hash|email|label>`
//! to filter the returned accounts (an id that matches nothing is a `404`
//! `AntigravityAccountNotFoundError`). `POST /api/antigravity/quotas` remains
//! the explicit refresh. Optional fields are omitted, never sent as `null`.

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{delete, get, post};
use oc_antigravity::{AccountStatusSummary, AntigravityError, AntigravityService};
use oc_types::{ApiError, DataEnvelope, now_millis};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::AppState;
use crate::data::{no_content, query_error};
use crate::error::ErrorResponse;

/// Query parameters for `GET /api/antigravity/quotas`.
#[derive(Debug, Default, Deserialize)]
pub struct QuotasQuery {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    refresh: Option<bool>,
}

/// Antigravity control-plane routes, merged into the protected router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/antigravity/accounts", get(list_accounts))
        .route("/api/antigravity/accounts/{id}", delete(remove_account))
        .route(
            "/api/antigravity/accounts/{id}/select",
            post(select_account),
        )
        .route(
            "/api/antigravity/accounts/{id}/enable",
            post(enable_account),
        )
        .route(
            "/api/antigravity/accounts/{id}/disable",
            post(disable_account),
        )
        .route(
            "/api/antigravity/quotas",
            get(get_quotas).post(refresh_quotas),
        )
        .route("/api/antigravity/models/refresh", post(refresh_models))
}

fn require_antigravity(state: &AppState) -> Result<&Arc<AntigravityService>, ErrorResponse> {
    state.antigravity().ok_or_else(|| {
        ErrorResponse::from(ApiError::service_unavailable(
            "antigravity service is not attached",
        ))
    })
}

/// Pool-level failures: an empty or unusable pool cannot satisfy the request,
/// which is a conflict with the pool state, not a missing resource (`404` stays
/// reserved for a specific account id that does not exist).
fn map_antigravity_error(err: &AntigravityError) -> ErrorResponse {
    match err {
        AntigravityError::NoAccounts(msg) => ApiError::new(409, "InvalidRequestError", msg.clone())
            .with("kind", json!("no_accounts"))
            .into(),
        AntigravityError::OAuth(msg) | AntigravityError::Config(msg) => {
            ApiError::new(400, "InvalidRequestError", msg.clone()).into()
        }
        other => ApiError::new(502, "ProviderError", other.to_string()).into(),
    }
}

/// Account-scoped failures: the named account does not exist in the pool.
fn map_account_error(err: &AntigravityError) -> ErrorResponse {
    match err {
        AntigravityError::NoAccounts(msg) => {
            ApiError::not_found("AntigravityAccountNotFoundError", msg.clone()).into()
        }
        other => map_antigravity_error(other),
    }
}

/// Whether a pool summary answers to `wanted` (index, hash, email or label).
fn account_matches(summary: &AccountStatusSummary, wanted: &str) -> bool {
    if wanted
        .parse::<usize>()
        .is_ok_and(|index| summary.index == index)
    {
        return true;
    }
    summary.id.eq_ignore_ascii_case(wanted)
        || summary
            .email
            .as_deref()
            .is_some_and(|email| email.eq_ignore_ascii_case(wanted))
        || summary
            .label
            .as_deref()
            .is_some_and(|label| label.eq_ignore_ascii_case(wanted))
}

/// `GET /api/antigravity/accounts`
pub async fn list_accounts(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    let accounts = svc
        .list_accounts_status()
        .map_err(|e| map_antigravity_error(&e))?;
    Ok(Json(DataEnvelope::new(json!({
        "accounts": account_wire_list(&accounts, now_millis()),
        "count": accounts.len(),
        "usableCount": svc.usable_account_count(),
        "version": svc.version().current(),
    }))))
}

/// Serialize account summaries for the wire, adding the derived
/// `cooldownUntilMs` field.
///
/// `AccountStatusSummary` lives in `oc-antigravity` and carries the raw
/// per-quota-key `rateLimits` map. Clients need one deadline to grey out a
/// cooling account, so this maps the pool summary onto JSON and adds the
/// maximum deadline that is still in the future. The field is omitted (never
/// `null`) for an account with no future deadline.
fn account_wire_list(accounts: &[AccountStatusSummary], now_ms: i64) -> Vec<Value> {
    accounts
        .iter()
        .map(|account| {
            let mut value = match serde_json::to_value(account) {
                Ok(value) => value,
                // `AccountStatusSummary` is a plain derive-Serialize struct with
                // no fallible field, so this cannot fail; if it ever does, the
                // account is skipped rather than reported with invented data.
                Err(error) => {
                    tracing::warn!(%error, "could not serialize antigravity account");
                    return Value::Null;
                }
            };
            let cooldown = account
                .rate_limits
                .values()
                .copied()
                .filter(|until| *until > now_ms)
                .max();
            if let (Some(until), Some(map)) = (cooldown, value.as_object_mut()) {
                map.insert("cooldownUntilMs".to_owned(), json!(until));
            }
            value
        })
        .filter(|value| !value.is_null())
        .collect()
}

/// `POST /api/antigravity/accounts/{id}/select`
pub async fn select_account(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    let summary = svc
        .select_account(&id)
        .await
        .map_err(|e| map_account_error(&e))?;
    Ok(Json(DataEnvelope::new(json!(summary))))
}

/// `POST /api/antigravity/accounts/{id}/enable`
pub async fn enable_account(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    let summary = svc
        .set_account_enabled(&id, true)
        .map_err(|e| map_account_error(&e))?;
    Ok(Json(DataEnvelope::new(json!(summary))))
}

/// `POST /api/antigravity/accounts/{id}/disable`
pub async fn disable_account(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    let summary = svc
        .set_account_enabled(&id, false)
        .map_err(|e| map_account_error(&e))?;
    Ok(Json(DataEnvelope::new(json!(summary))))
}

/// `DELETE /api/antigravity/accounts/{id}`
pub async fn remove_account(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    svc.remove_account(&id)
        .await
        .map_err(|e| map_account_error(&e))?;
    Ok(no_content())
}

/// `GET /api/antigravity/quotas`
///
/// A read: the cached snapshot is served with `accounts` and `discoveredModels`.
/// `?refresh=true` also runs the live probe; when it succeeds the truthful
/// `report` is added, and when it fails the read still answers with the cached
/// snapshot plus an `error` string — the failure is reported, never hidden and
/// never dressed up as a refresh. `?id=` filters the pool by index, hash, email
/// or label; an id that matches no account is `404`
/// `AntigravityAccountNotFoundError`.
pub async fn get_quotas(
    State(state): State<AppState>,
    params: Result<Query<QuotasQuery>, QueryRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let svc = require_antigravity(&state)?;
    let mut report = None;
    let mut refresh_error = None;
    if params.refresh.unwrap_or(false) {
        match svc.refresh_quotas_and_models().await {
            Ok(refreshed) => report = Some(refreshed),
            Err(error) => refresh_error = Some(error.to_string()),
        }
    }
    let mut accounts = match &report {
        Some(report) => report.accounts.clone(),
        None => svc
            .list_accounts_status()
            .map_err(|e| map_antigravity_error(&e))?,
    };
    if let Some(wanted) = params
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        accounts.retain(|summary| account_matches(summary, wanted));
        if accounts.is_empty() {
            return Err(ErrorResponse::from(ApiError::not_found(
                "AntigravityAccountNotFoundError",
                format!("Antigravity account not found: {wanted}"),
            )));
        }
    }
    let mut data = Map::new();
    data.insert("accounts".to_owned(), json!(accounts));
    data.insert(
        "discoveredModels".to_owned(),
        json!(svc.cached_discovered_models()),
    );
    if let Some(report) = report {
        data.insert("report".to_owned(), json!(report));
    }
    if let Some(error) = refresh_error {
        data.insert("error".to_owned(), Value::String(error));
    }
    Ok(Json(DataEnvelope::new(Value::Object(data))))
}

/// `POST /api/antigravity/quotas`
pub async fn refresh_quotas(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    let report = svc
        .refresh_quotas_and_models()
        .await
        .map_err(|e| map_antigravity_error(&e))?;
    Ok(Json(DataEnvelope::new(json!(report))))
}

/// `POST /api/antigravity/models/refresh`
///
/// Quota inspection and model discovery share one upstream probe
/// (`fetchAvailableModels` is what reports the model inventory), so this
/// endpoint always runs `refresh_quotas_and_models` and returns the truthful
/// `report` next to the freshly discovered `models`. Nothing is reported as
/// refreshed unless the probe actually ran; a failure is the mapped error.
pub async fn refresh_models(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let svc = require_antigravity(&state)?;
    let report = svc
        .refresh_quotas_and_models()
        .await
        .map_err(|e| map_antigravity_error(&e))?;
    let models = svc.antigravity_model_infos();
    Ok(Json(DataEnvelope::new(json!({
        "report": report,
        "models": models,
    }))))
}
