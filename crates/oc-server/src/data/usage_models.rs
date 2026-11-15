//! `GET /api/experimental/usage/models?days=7`.
//!
//! Model-level usage rollup over `session_message.data` (no new tables). The
//! unscoped scan is the indexed `oc_store` rollup shared with the Models
//! registry ([`super::models_registry::model_usage_since`]).

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use oc_types::DataEnvelope;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::data::models_registry::model_usage_since;
use crate::data::query_error;
use crate::error::ErrorResponse;

const DEFAULT_DAYS: i64 = 7;
const MIN_DAYS: i64 = 1;
const MAX_DAYS: i64 = 90;
const MAX_ROWS: usize = 200;
const DAY_MS: i64 = 86_400_000;

/// `?days=` query.
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    days: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelsBody {
    window_days: i64,
    models: Vec<UsageModelRow>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelRow {
    #[serde(rename = "providerID")]
    provider_id: String,
    id: String,
    requests: i64,
    tokens_input: i64,
    tokens_output: i64,
    tokens_reasoning: i64,
    cost_usd: f64,
}

pub async fn usage_models(
    State(state): State<AppState>,
    params: Result<Query<UsageQuery>, QueryRejection>,
) -> Result<Json<DataEnvelope<UsageModelsBody>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let days = params
        .days
        .unwrap_or(DEFAULT_DAYS)
        .clamp(MIN_DAYS, MAX_DAYS);
    let since = oc_types::now_millis() - days * DAY_MS;
    let scan = model_usage_since(&state, since, None).await?;
    let mut models: Vec<UsageModelRow> = scan
        .into_iter()
        .map(|((provider_id, id), accum)| UsageModelRow {
            provider_id,
            id,
            requests: accum.requests,
            tokens_input: accum.tokens_input,
            tokens_output: accum.tokens_output,
            tokens_reasoning: accum.tokens_reasoning,
            cost_usd: (accum.cost_usd * 100.0).round() / 100.0,
        })
        .collect();
    models.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| a.provider_id.cmp(&b.provider_id))
            .then_with(|| a.id.cmp(&b.id))
    });
    models.truncate(MAX_ROWS);
    Ok(Json(DataEnvelope::new(UsageModelsBody {
        window_days: days,
        models,
    })))
}
