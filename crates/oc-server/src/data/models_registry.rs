//! Model control plane: `GET /api/models/registry` and `PUT /api/config/models`.
//!
//! The wire contract is frozen in `docs/02-protocol.md` ("Model control plane")
//! and `docs/17-interfaces.md`. The registry is one call for the Models tab:
//! the catalog models with visibility/favorite/note, the last-120 h usage
//! rollup and the configured quotas, plus the `models` config block echo.
//!
//! Both handlers read the live `oc_config::ConfigService` models cell through
//! [`effective_models`] (`ConfigService::models_section`), so the block a PUT
//! published is the block the next GET describes; nothing is re-read from disk
//! on read.
//!
//! [`put_models`] is typed, validates every model reference against the catalog
//! **before** touching the file, then deep-merges the supplied keys onto the
//! `models` block read back from disk and rewrites the document atomically
//! through the shared writer in [`crate::data::config_patch`]. The committed
//! block is then pushed into the models cell (`ConfigService::set_models`,
//! which bumps `models_revision`) and the response echo is built by the same
//! path as a GET from that cell, so PUT and GET cannot disagree.
//!
//! A change takes effect without a restart: the PUT response carries
//! `Cache-Control: no-store` and `x-opencode-config-reloaded: models`. Only an
//! external edit of the config file still needs one — nothing watches that
//! file, so it reaches the cell on the next process start or an explicit
//! `ConfigService::refresh_models_from_disk` call.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, HeaderName};
use axum::response::{IntoResponse, Response};
use oc_llm::{Catalog, Cost, CostCache, Limit, ModelInfo};
use oc_types::{ApiError, DataEnvelope};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::model::{
    available_provider_ids_with_credentials, filtered_catalog_models,
    has_direct_google_access_with_credentials, load_credentials,
};
use crate::data::{query_error, require_catalog, require_config, schema_error};
use crate::error::ErrorResponse;

/// Window the per-model usage rollup reports when `models.quota.window_hours`
/// is absent, in hours.
const USAGE_WINDOW_HOURS: i64 = 120;
/// Bound on the registry model list, so a pathological catalog cannot produce
/// an unbounded response.
const MAX_MODELS: usize = 1000;
/// Bound on one fallback chain, in entries, on write and on the echo.
const MAX_CHAIN: usize = 16;
/// Bound on the session tree walk a `sessionID` scope may cover.
const MAX_SUBTREE_SESSIONS: usize = 500;
/// Documented default reserve share when `models.quota.reserve_percent` is absent.
const DEFAULT_RESERVE_PERCENT: i64 = 10;
/// Documented default quota window when `models.quota.window_hours` is absent.
const DEFAULT_QUOTA_WINDOW_HOURS: i64 = 5;
/// Documented default hop limit on the wire when `fallback.max_hops` is absent.
const DEFAULT_MAX_HOPS: u32 = 2;

// ---------------------------------------------------------------------------
// Wire bodies
// ---------------------------------------------------------------------------

/// `GET /api/models/registry` body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryBody {
    models: Vec<RegistryModel>,
    chain: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    chains: BTreeMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<FallbackPolicy>,
    /// Classes an explicit `models.fallback.on` switched off, so the Models tab
    /// can warn that a failure class will not advance the chain. Only the
    /// opt-in classes (`empty_response`, `transport`) can appear: quota and
    /// rate limit always advance. Omitted when the configured set is complete.
    #[serde(rename = "fallbackDisabled", skip_serializing_if = "Vec::is_empty")]
    fallback_disabled: Vec<&'static str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    agent_notes: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<QuotaConfig>,
    usage: RegistryUsage,
    /// Models the runtime currently considers unavailable (a recent failure in a
    /// model-level class). Omitted when every model is healthy, never null.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    model_health: Vec<RegistryHealth>,
    /// Per-credential cooldowns the runtime holds. A key here is out of rotation
    /// for the *same* model until `untilMs`; it never hides a model. Omitted when
    /// every key is healthy, never null.
    #[serde(rename = "keyHealth", skip_serializing_if = "Vec::is_empty")]
    key_health: Vec<RegistryKeyHealth>,
}

/// One entry of the top-level `keyHealth` list.
///
/// This is the credential axis, not the model axis: [`RegistryHealth`] retires a
/// model for everyone, while this only takes one key out of rotation for one
/// provider. `credentialID` is omitted for a provider's environment key, which
/// has no stored credential row.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistryKeyHealth {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "credentialID", skip_serializing_if = "Option::is_none")]
    credential_id: Option<String>,
    until_ms: i64,
    kind: String,
    failures: u32,
}

/// One entry of the top-level `modelHealth` list.
///
/// Mirrors `oc_llm::Unavailable` on the wire: `untilMs` is epoch milliseconds,
/// `kind` is the `FailureKind` wire string, and `message` is omitted when the
/// runtime did not record one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistryHealth {
    #[serde(rename = "providerID")]
    provider_id: String,
    id: String,
    reference: String,
    until_ms: i64,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Per-model health badge: the same information as [`RegistryHealth`] without
/// the identifying fields the row already carries.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistryUnavailable {
    until_ms: i64,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistryModel {
    #[serde(rename = "providerID")]
    provider_id: String,
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    variants: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cost: Vec<Cost>,
    limit: Limit,
    visible: bool,
    favorite: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// Usage rollup for the configured window. Sparse: emitted only when the
    /// model has at least one assistant message in the window, and omitted
    /// entirely under `?usage=0`. A model with no rows has no `usage` field,
    /// never a zero-filled one (docs/15-performance.md registry budget).
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<ModelUsage>,
    /// Per-model quota, present only when a rule exists for this reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<ModelQuota>,
    /// Present while the runtime holds this model unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable: Option<RegistryUnavailable>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelUsage {
    requests: i64,
    tokens_input: i64,
    tokens_output: i64,
    tokens_reasoning: i64,
    cost_usd: f64,
    window_hours: i64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelQuota {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_requests: Option<i64>,
    used_cost_usd: f64,
    used_requests: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_percent: Option<f64>,
    limited: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FallbackPolicy {
    on: Vec<String>,
    max_hops: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    reserve_percent: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_hours: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<QuotaRule>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    models: BTreeMap<String, QuotaRule>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotaRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cost_usd: Option<f64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "max_requests_patch"
    )]
    max_requests: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistryUsage {
    window_hours: i64,
    total_cost_usd: f64,
    total_requests: i64,
}

// ---------------------------------------------------------------------------
// Write body
// ---------------------------------------------------------------------------

/// The `models` config subset `PUT /api/config/models` accepts.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelsPatch {
    #[serde(default)]
    chain: Option<Vec<String>>,
    #[serde(default)]
    chains: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    fallback: Option<FallbackPatch>,
    #[serde(default)]
    notes: Option<BTreeMap<String, String>>,
    #[serde(default)]
    agent_notes: Option<BTreeMap<String, BTreeMap<String, String>>>,
    #[serde(default)]
    hidden: Option<Vec<String>>,
    #[serde(default)]
    favorites: Option<Vec<String>>,
    #[serde(default)]
    pricing: Option<BTreeMap<String, PricingEntry>>,
    #[serde(default)]
    quota: Option<QuotaPatch>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FallbackPatch {
    #[serde(default)]
    on: Option<Vec<String>>,
    #[serde(default)]
    max_hops: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PricingEntry {
    input: f64,
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuotaPatch {
    #[serde(default, deserialize_with = "reserve_percent_patch")]
    reserve_percent: Option<i64>,
    #[serde(default, deserialize_with = "window_hours_patch")]
    window_hours: Option<i64>,
    #[serde(default)]
    default: Option<QuotaRule>,
    #[serde(default)]
    models: Option<BTreeMap<String, QuotaRule>>,
}

/// Decode an optional integer config field, accepting an integral float.
///
/// JSON has no int/float distinction and this route used to persist
/// `reserve_percent` as `10.0`; `10` and `10.0` are the same quota value to
/// `oc-runtime`'s `parse_quota`, so the writer normalizes to the integer it
/// documents. A fractional number is rounded rather than refused (the route
/// accepted it before, and a rounded integer is always loadable), and the value
/// is clamped to the documented range with the same bounds `parse_quota`
/// applies, so a successful `PUT` always leaves a document the loader accepts.
fn integer_field<'de, D>(deserializer: D, min: i64, max: i64) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Integer(i64),
        Float(f64),
    }

    let Some(number) = Option::<Number>::deserialize(deserializer)? else {
        return Ok(None);
    };
    // `f64 -> i64` casts saturate (NaN maps to 0), so one conversion covers
    // non-finite and out-of-range input; clamping afterwards in integer space
    // applies the documented range without a lossy `i64 -> f64` conversion.
    #[allow(clippy::cast_possible_truncation)]
    Ok(Some(match number {
        Number::Integer(value) => value.clamp(min, max),
        Number::Float(value) => (value.round() as i64).clamp(min, max),
    }))
}

/// `quota.reservePercent` — documented `0..=100`.
fn reserve_percent_patch<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    integer_field(deserializer, 0, 100)
}

/// `quota.windowHours` — documented `1..=720`.
fn window_hours_patch<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    integer_field(deserializer, 1, 720)
}

/// `quota.*.maxRequests` — a non-negative request count, unbounded above.
fn max_requests_patch<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    integer_field(deserializer, 0, i64::MAX)
}

impl QuotaRule {
    fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let rule = Self {
            max_cost_usd: number_f64(
                object
                    .get("max_cost_usd")
                    .or_else(|| object.get("maxCostUsd")),
            ),
            max_requests: number_i64(
                object
                    .get("max_requests")
                    .or_else(|| object.get("maxRequests")),
            ),
        };
        (rule.max_cost_usd.is_some() || rule.max_requests.is_some()).then_some(rule)
    }
}

impl PricingEntry {
    fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        Some(Self {
            input: number_f64(object.get("input")).unwrap_or(0.0),
            output: number_f64(object.get("output")).unwrap_or(0.0),
            cache_read: number_f64(object.get("cacheRead").or_else(|| object.get("cache_read"))),
            cache_write: number_f64(
                object
                    .get("cacheWrite")
                    .or_else(|| object.get("cache_write")),
            ),
        })
    }

    /// The catalog price replaced by this override (USD per 1M tokens).
    fn cost(self) -> Vec<Cost> {
        vec![Cost {
            tier: None,
            input: self.input,
            output: self.output,
            cache: CostCache {
                read: self.cache_read.unwrap_or(0.0),
                write: self.cache_write.unwrap_or(0.0),
            },
        }]
    }
}

// ---------------------------------------------------------------------------
// Parsed config
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ParsedQuota {
    configured: bool,
    reserve_percent: Option<i64>,
    window_hours: Option<i64>,
    default: Option<QuotaRule>,
    models: BTreeMap<String, QuotaRule>,
}

impl ParsedQuota {
    fn parse(value: Option<&Value>) -> Self {
        let Some(object) = value.and_then(Value::as_object) else {
            return Self::default();
        };
        let models = object
            .get("models")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(reference, rule)| {
                        Some((reference.clone(), QuotaRule::parse(rule)?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            configured: true,
            reserve_percent: number_i64(
                object
                    .get("reserve_percent")
                    .or_else(|| object.get("reservePercent")),
            )
            // Same clamp as the runtime parser: the echo must not advertise a
            // reserve the loader would refuse or reduce to a different value.
            .map(|percent| percent.clamp(0, 100)),
            window_hours: number_i64(
                object
                    .get("window_hours")
                    .or_else(|| object.get("windowHours")),
            )
            .filter(|hours| *hours > 0),
            default: object.get("default").and_then(QuotaRule::parse),
            models,
        }
    }

    fn has_budget(&self) -> bool {
        self.default.is_some() || !self.models.is_empty()
    }

    fn rule(&self, reference: &str) -> Option<QuotaRule> {
        self.models.get(reference).copied().or(self.default)
    }

    fn effective_reserve_percent(&self) -> i64 {
        self.reserve_percent.unwrap_or(DEFAULT_RESERVE_PERCENT)
    }

    fn effective_window_hours(&self) -> i64 {
        self.window_hours.unwrap_or(DEFAULT_QUOTA_WINDOW_HOURS)
    }

    fn to_wire(&self) -> Option<QuotaConfig> {
        self.configured.then(|| QuotaConfig {
            reserve_percent: self.reserve_percent,
            window_hours: self.window_hours,
            default: self.default,
            models: self.models.clone(),
        })
    }
}

#[derive(Debug, Default)]
struct ParsedModels {
    chain: Vec<String>,
    chains: BTreeMap<String, Vec<String>>,
    fallback: Option<FallbackPolicy>,
    notes: BTreeMap<String, String>,
    agent_notes: BTreeMap<String, BTreeMap<String, String>>,
    hidden: BTreeSet<String>,
    favorites: BTreeSet<String>,
    pricing: BTreeMap<String, PricingEntry>,
    quota: ParsedQuota,
}

impl ParsedModels {
    fn parse(value: &Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::default();
        };
        let chains = object
            .get("chains")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(agent, chain)| {
                        let mut chain = string_list(Some(chain));
                        chain.truncate(MAX_CHAIN);
                        (agent.clone(), chain)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fallback = object
            .get("fallback")
            .and_then(Value::as_object)
            .map(|map| FallbackPolicy {
                on: string_list(map.get("on")),
                max_hops: number_u32(map.get("max_hops").or_else(|| map.get("maxHops")))
                    .unwrap_or(DEFAULT_MAX_HOPS),
            });
        let agent_notes = object
            .get("agent_notes")
            .or_else(|| object.get("agentNotes"))
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(agent, notes)| (agent.clone(), string_map(Some(notes))))
                    .filter(|(_, notes)| !notes.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let pricing = object
            .get("pricing")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(reference, entry)| {
                        Some((reference.clone(), PricingEntry::parse(entry)?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut chain = string_list(object.get("chain"));
        chain.truncate(MAX_CHAIN);
        Self {
            chain,
            chains,
            fallback,
            notes: string_map(object.get("notes")),
            agent_notes,
            hidden: string_list(object.get("hidden")).into_iter().collect(),
            favorites: string_list(object.get("favorites")).into_iter().collect(),
            pricing,
            quota: ParsedQuota::parse(object.get("quota")),
        }
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, item)| Some((key.clone(), item.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

fn number_f64(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

fn number_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    // JSON has no int/float distinction: `10.0` is the same quota value as
    // `10`, and a fractional number is rounded so the echo (and the quota math
    // built on it) never silently falls back to the default.
    let number = value.as_f64().filter(|number| number.is_finite())?;
    #[allow(clippy::cast_possible_truncation)]
    let rounded = number.round() as i64;
    Some(rounded)
}

fn number_u32(value: Option<&Value>) -> Option<u32> {
    value
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}

// ---------------------------------------------------------------------------
// Usage scan (bounded, shared with `usage_models`)
// ---------------------------------------------------------------------------

/// Per-(provider, model) usage totals over one window.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UsageAccum {
    pub(crate) requests: i64,
    pub(crate) tokens_input: i64,
    pub(crate) tokens_output: i64,
    pub(crate) tokens_reasoning: i64,
    pub(crate) cost_usd: f64,
}

impl UsageAccum {
    /// True when the model recorded no request in the window.
    ///
    /// The registry omits `usage` for such a model rather than emitting a
    /// zero-filled object: 689 of 692 catalog models have no rows, and their
    /// usage objects were 75 KB of the 251 KB body (docs/15-performance.md).
    fn is_empty(&self) -> bool {
        self.requests == 0
            && self.tokens_input == 0
            && self.tokens_output == 0
            && self.tokens_reasoning == 0
            && self.cost_usd == 0.0
    }
}

/// Roll up assistant-message usage since `since_ms`, optionally limited to the
/// session ids in `scope`.
///
/// Uses SQLite `json_extract` aggregation instead of decoding message JSON trees.
pub(crate) async fn model_usage_since(
    state: &AppState,
    since_ms: i64,
    scope: Option<&[String]>,
) -> Result<BTreeMap<(String, String), UsageAccum>, ErrorResponse> {
    if scope.is_some() {
        return scan_usage(state, since_ms, scope).await;
    }
    let Some(store) = state.store() else {
        return Ok(BTreeMap::new());
    };
    let rows = store.model_usage(since_ms).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                (row.provider_id, row.model_id),
                UsageAccum {
                    requests: row.requests,
                    tokens_input: row.tokens_in,
                    tokens_output: row.tokens_out,
                    tokens_reasoning: row.tokens_reasoning,
                    cost_usd: row.cost,
                },
            )
        })
        .collect())
}

/// Roll up assistant-message usage since `since_ms`, optionally limited to the
/// session ids in `scope`.
pub(crate) async fn scan_usage(
    state: &AppState,
    since_ms: i64,
    scope: Option<&[String]>,
) -> Result<BTreeMap<(String, String), UsageAccum>, ErrorResponse> {
    let Some(store) = state.store() else {
        return Ok(BTreeMap::new());
    };
    let rows = match scope {
        None => store.model_usage(since_ms).await?,
        Some([]) => return Ok(BTreeMap::new()),
        Some(ids) => {
            let mut combined = Vec::new();
            for chunk in ids.chunks(MAX_SUBTREE_SESSIONS) {
                combined.extend(store.model_usage_for_sessions(chunk, since_ms).await?);
            }
            combined
        }
    };
    let mut usage: BTreeMap<(String, String), UsageAccum> = BTreeMap::new();
    for row in rows {
        let accum = usage.entry((row.provider_id, row.model_id)).or_default();
        accum.requests += row.requests;
        accum.tokens_input += row.tokens_in;
        accum.tokens_output += row.tokens_out;
        accum.tokens_reasoning += row.tokens_reasoning;
        accum.cost_usd += row.cost;
    }
    Ok(usage)
}

/// Ids of `session_id` and its descendants (bounded by [`MAX_SUBTREE_SESSIONS`]).
async fn session_scope(state: &AppState, session_id: &str) -> Result<Vec<String>, ErrorResponse> {
    let store = state
        .store()
        .ok_or_else(|| ApiError::service_unavailable("data store not available"))?;
    let node = store
        .session_node(session_id)
        .await?
        .ok_or_else(|| ErrorResponse::from(crate::data::session_not_found(session_id)))?;
    let mut ids = vec![node.id.clone()];
    let mut frontier = vec![node.id];
    while let Some(id) = frontier.pop() {
        for child in store.children(&id).await? {
            if ids.len() >= MAX_SUBTREE_SESSIONS {
                return Ok(ids);
            }
            ids.push(child.id.clone());
            frontier.push(child.id);
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/models/registry?sessionID=`
#[derive(Debug, Deserialize)]
pub struct RegistryQuery {
    #[serde(rename = "sessionID", default)]
    session_id: Option<String>,
    /// `?usage=0` (or `false`) omits the per-model usage rollup. The Models tab
    /// paints with `usage=0` and refetches with usage when the usage section
    /// opens; absent or `1` keeps the full body (docs/15-performance.md).
    #[serde(default)]
    usage: Option<String>,
}

pub async fn registry(
    State(state): State<AppState>,
    params: Result<Query<RegistryQuery>, QueryRejection>,
) -> Result<Json<DataEnvelope<RegistryBody>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let include_usage = params
        .usage
        .as_deref()
        .is_none_or(|value| !matches!(value.trim(), "0" | "false"));
    let body = build_registry(&state, params.session_id.as_deref(), include_usage).await?;
    Ok(Json(DataEnvelope::new(body)))
}

/// `PUT /api/config/models`
pub async fn put_models(
    State(state): State<AppState>,
    body: Result<Json<ModelsPatch>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(patch) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let catalog = require_catalog(&state)?;
    validate_model_refs(&state, catalog.as_ref(), &patch)?;

    // `update_document` re-reads the file under its lock and the closure below
    // deep-merges onto that on-disk block, so two PUTs in one process cannot
    // resurrect a value the first one replaced. The merged block is handed back
    // so the config cell can be updated with exactly what was committed.
    let write_value = patch_to_config(&patch);
    let dir = crate::data::config_patch::write_dir(&state);
    let written = tokio::task::spawn_blocking(move || {
        let mut written = Value::Object(Map::new());
        crate::data::config_patch::update_document(&dir, |document| {
            let current = document
                .get("models")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            if !current.is_object() {
                return Err(ApiError::bad_request(
                    "existing config models is not an object",
                ));
            }
            let merged = deep_merge(&current, &write_value);
            document.insert("models".to_owned(), merged.clone());
            written = merged;
            Ok(())
        })?;
        Ok::<Value, ApiError>(written)
    })
    .await
    .map_err(|error| {
        ErrorResponse::from(ApiError::internal(format!(
            "config write task failed: {error}"
        )))
    })?
    .map_err(ErrorResponse::from)?;

    // Publish the committed block to the live cell. This is what makes the PUT
    // take effect for the next spawn AND for the echo below without a restart:
    // the runtime re-parses `models` per spawn from this cell, and the registry
    // reads the same cell. An external edit to the file still needs a restart
    // (nothing watches it).
    if let Some(config) = state.config() {
        config.set_models(written);
    }

    // The echo is built by the same path as a later GET, from the cell just
    // updated, so PUT→GET cannot disagree.
    // The PUT echo keeps the full body: the UI reconciles the saved policy
    // against real usage, and a save is not the first paint.
    let registry = build_registry(&state, None, true).await?;
    let mut response = Json(DataEnvelope::new(registry)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // No restart banner: the change is live. The reload hint tells a client what
    // was applied.
    response.headers_mut().insert(
        HeaderName::from_static("x-opencode-config-reloaded"),
        HeaderValue::from_static("models"),
    );
    Ok(response)
}

// ---------------------------------------------------------------------------
// Assembly
// ---------------------------------------------------------------------------

/// The `models` block the registry should describe.
///
/// This reads the live [`oc_config::ConfigService`] cell, which
/// `PUT /api/config/models` replaces in place ([`ConfigService::set_models`]),
/// so a PUT is visible to the very next GET without a restart. External edits to
/// the config file are NOT picked up here: they land on the next process start
/// (`ConfigService::refresh_models_from_disk` is only called on demand).
fn effective_models(state: &AppState) -> Result<Value, ErrorResponse> {
    Ok(require_config(state)?.models_section())
}

#[allow(clippy::too_many_lines)]
async fn build_registry(
    state: &AppState,
    session_id: Option<&str>,
    include_usage: bool,
) -> Result<RegistryBody, ErrorResponse> {
    let catalog = require_catalog(state)?;
    let models_config = effective_models(state)?;
    let parsed = ParsedModels::parse(&models_config);
    // Read through the same policy the runtime spawns with, so the warning can
    // never disagree with which classes actually advance the chain.
    let policy = oc_runtime::models_policy(require_config(state)?);
    let fallback_disabled = policy.fallback().disabled_by_config();

    let credentials = load_credentials(state).await;
    let available = available_provider_ids_with_credentials(state, catalog.as_ref(), &credentials);
    let direct_google =
        has_direct_google_access_with_credentials(state, catalog.as_ref(), &credentials);
    let (filtered_models, _) =
        filtered_catalog_models(state, catalog.as_ref(), &available, direct_google);
    let mut infos: Vec<ModelInfo> = (*filtered_models).clone();
    infos.sort_by(|a, b| {
        a.provider_id
            .cmp(&b.provider_id)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });

    let now = oc_types::now_millis();
    // Runtime health, keyed variant-insensitively (`provider/id`, lowercased,
    // '.' -> '-') exactly like `oc_llm::ModelHealth` itself.
    let health = state
        .model_health()
        .map(|health| health.entries(now))
        .unwrap_or_default();
    let mut health_map = HashMap::with_capacity(health.len());
    for (seen, entry) in &health {
        health_map.entry(seen.as_str()).or_insert(entry);
    }
    let health_key = |reference: &str| -> String {
        reference
            .split_once('#')
            .map_or(reference, |(base, _)| base)
            .to_ascii_lowercase()
            .replace('.', "-")
    };
    let unavailable_of = |reference: &str| -> Option<RegistryUnavailable> {
        let key = health_key(reference);
        health_map
            .get(key.as_str())
            .map(|entry| RegistryUnavailable {
                until_ms: entry.until_ms,
                kind: entry.kind.as_str().to_owned(),
                message: entry.message.clone(),
            })
    };
    let scope = match session_id {
        Some(id) => Some(session_scope(state, id).await?),
        None => None,
    };
    // The echo window follows `models.quota.window_hours` when configured and
    // defaults to 120 h; per-model quota budgets keep their own 5 h default.
    let usage_hours = parsed.quota.window_hours.unwrap_or(USAGE_WINDOW_HOURS);
    // `?usage=0` skips the rollup entirely for the first paint. The quota pass
    // below is independent and stays: `quota.used*` must be correct either way.
    let usage = if include_usage {
        model_usage_since(state, now - usage_hours * 3_600_000, scope.as_deref()).await?
    } else {
        BTreeMap::new()
    };
    let quota_usage = if parsed.quota.has_budget() {
        let window = parsed.quota.effective_window_hours();
        Some(model_usage_since(state, now - window * 3_600_000, scope.as_deref()).await?)
    } else {
        None
    };
    let reserve_percent = parsed.quota.effective_reserve_percent();
    let key_health = state
        .key_pool()
        .map(|pool| pool.entries(now))
        .unwrap_or_default();

    let mut models = Vec::with_capacity(infos.len());
    for info in infos {
        let key = (info.provider_id.clone(), info.id.clone());
        let reference = format!("{}/{}", info.provider_id, info.id);
        let accum = usage.get(&key).copied().unwrap_or_default();
        let quota = parsed.quota.rule(&reference).map(|rule| {
            let used = quota_usage
                .as_ref()
                .and_then(|scan| scan.get(&key))
                .copied()
                .unwrap_or_default();
            model_quota(rule, used, reserve_percent)
        });
        let cost = parsed
            .pricing
            .get(&reference)
            .map_or_else(|| info.cost.clone(), |entry| entry.cost());
        let note = parsed
            .notes
            .get(&reference)
            .filter(|note| !note.is_empty())
            .cloned();
        models.push(RegistryModel {
            visible: !parsed.hidden.contains(&reference),
            favorite: parsed.favorites.contains(&reference),
            provider_id: info.provider_id,
            id: info.id,
            name: info.name,
            variants: info
                .variants
                .iter()
                .map(|variant| variant.id.clone())
                .collect(),
            cost,
            limit: info.limit,
            note,
            usage: (!accum.is_empty()).then(|| ModelUsage {
                requests: accum.requests,
                tokens_input: accum.tokens_input,
                tokens_output: accum.tokens_output,
                tokens_reasoning: accum.tokens_reasoning,
                cost_usd: round_cents(accum.cost_usd),
                window_hours: usage_hours,
            }),
            quota,
            unavailable: unavailable_of(&reference),
        });
    }

    let mut total_cost_usd = 0.0;
    let mut total_requests = 0;
    for accum in usage.values() {
        total_cost_usd += accum.cost_usd;
        total_requests += accum.requests;
    }
    models.truncate(MAX_MODELS);

    Ok(RegistryBody {
        models,
        chain: parsed.chain,
        chains: parsed.chains,
        fallback: parsed.fallback,
        fallback_disabled,
        agent_notes: parsed.agent_notes,
        quota: parsed.quota.to_wire(),
        usage: RegistryUsage {
            window_hours: usage_hours,
            total_cost_usd: round_cents(total_cost_usd),
            total_requests,
        },
        model_health: health
            .iter()
            .map(|(reference, entry)| {
                let (provider_id, id) = reference
                    .split_once('/')
                    .map_or((reference.as_str(), ""), |(provider, id)| (provider, id));
                RegistryHealth {
                    provider_id: provider_id.to_owned(),
                    id: id.to_owned(),
                    reference: reference.clone(),
                    until_ms: entry.until_ms,
                    kind: entry.kind.as_str().to_owned(),
                    message: entry.message.clone(),
                }
            })
            .collect(),
        key_health: key_health
            .into_iter()
            .map(|entry| RegistryKeyHealth {
                provider_id: entry.provider,
                credential_id: entry.credential_id,
                until_ms: entry.until_ms,
                kind: entry.kind.as_str().to_owned(),
                failures: entry.failures,
            })
            .collect(),
    })
}

// Request counts are tiny relative to f64's integer range, so the lossy cast
// is bounded by construction; clippy cannot see that.
#[allow(clippy::cast_precision_loss)]
fn model_quota(rule: QuotaRule, used: UsageAccum, reserve_percent: i64) -> ModelQuota {
    // Mirrors the spawn resolver (`crates/oc-runtime/src/subagent/spawn/prepare.rs`):
    // a model is spent once `used >= cap * (100 - reserve) / 100`, for EITHER
    // cap. On the "share of the cap still free" scale that is exactly
    // `remaining <= reserve`, so both caps must be evaluated and the smaller
    // remaining share wins — a cost cap must never mask an exhausted request
    // cap, or the UI reads "healthy" while every spawn is refused.
    // `reserve_percent` is clamped to 0..=100 on read, so the f64 conversion is
    // exact for every value the field can hold.
    let reserve = reserve_percent as f64;
    let cost_percent = rule
        .max_cost_usd
        .filter(|max| *max > 0.0)
        .map(|max| 100.0 * (1.0 - used.cost_usd / max));
    let requests_percent = rule
        .max_requests
        .filter(|max| *max > 0)
        .map(|max| 100.0 * (1.0 - used.requests as f64 / max as f64));
    let cost_denied = cost_percent.is_some_and(|percent| percent <= reserve);
    let requests_denied = requests_percent.is_some_and(|percent| percent <= reserve);
    let remaining_percent = match (cost_percent, requests_percent) {
        (Some(cost), Some(requests)) => Some(cost.min(requests)),
        (cost, requests) => cost.or(requests),
    }
    .map(|percent| round_tenth(percent.clamp(0.0, 100.0)));
    ModelQuota {
        max_cost_usd: rule.max_cost_usd,
        max_requests: rule.max_requests,
        used_cost_usd: round_cents(used.cost_usd),
        used_requests: used.requests,
        remaining_percent,
        limited: cost_denied || requests_denied,
    }
}
fn round_cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round_tenth(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

// ---------------------------------------------------------------------------
// Validation and merge
// ---------------------------------------------------------------------------

fn validate_model_refs(
    state: &AppState,
    catalog: &Catalog,
    patch: &ModelsPatch,
) -> Result<(), ApiError> {
    // Bounded collections: a chain longer than `MAX_CHAIN` is refused rather
    // than silently truncated, so the file and the registry echo always agree.
    if let Some(chain) = &patch.chain
        && chain.len() > MAX_CHAIN
    {
        return Err(chain_too_long(chain.len()));
    }
    if let Some(chains) = &patch.chains {
        for (agent, chain) in chains {
            if chain.len() > MAX_CHAIN {
                return Err(chain_too_long(chain.len()).with("agent", json!(agent)));
            }
        }
    }
    let mut references: Vec<&str> = Vec::new();
    if let Some(chain) = &patch.chain {
        references.extend(chain.iter().map(String::as_str));
    }
    if let Some(chains) = &patch.chains {
        for chain in chains.values() {
            references.extend(chain.iter().map(String::as_str));
        }
    }
    if let Some(notes) = &patch.notes {
        references.extend(notes.keys().map(String::as_str));
    }
    if let Some(agent_notes) = &patch.agent_notes {
        for notes in agent_notes.values() {
            references.extend(notes.keys().map(String::as_str));
        }
    }
    if let Some(hidden) = &patch.hidden {
        references.extend(hidden.iter().map(String::as_str));
    }
    if let Some(favorites) = &patch.favorites {
        references.extend(favorites.iter().map(String::as_str));
    }
    if let Some(pricing) = &patch.pricing {
        references.extend(pricing.keys().map(String::as_str));
    }
    if let Some(quota) = &patch.quota
        && let Some(models) = &quota.models
    {
        references.extend(models.keys().map(String::as_str));
    }
    for reference in references {
        let resolved = catalog.resolve(reference).is_ok()
            || state
                .antigravity()
                .is_some_and(|service| service.resolve_catalog_model(catalog, reference).is_some());
        if !resolved {
            // docs/02-protocol.md and docs/14-config-permissions.md freeze this
            // as `400 InvalidRequestError`; the status table has no 422.
            return Err(ApiError::new(
                400,
                "InvalidRequestError",
                format!("invalid_model: {reference}"),
            )
            .with("model", json!(reference)));
        }
    }
    Ok(())
}

/// One chain longer than [`MAX_CHAIN`] entries.
fn chain_too_long(len: usize) -> ApiError {
    // Same tagged error as an unknown ref: the body is well formed but one of
    // its refs cannot be honoured (docs/02-protocol.md has no 422).
    ApiError::new(
        400,
        "InvalidRequestError",
        format!("models chain has {len} entries; the limit is {MAX_CHAIN}"),
    )
}

/// Convert the typed patch into the canonical config shapes of
/// `docs/14-config-permissions.md` (`fallback`/`quota` `snake_case`, `pricing`
/// `camelCase` inside the entry).
fn patch_to_config(patch: &ModelsPatch) -> Value {
    let mut object = Map::new();
    if let Some(chain) = &patch.chain {
        object.insert("chain".to_owned(), json!(chain));
    }
    if let Some(chains) = &patch.chains {
        object.insert("chains".to_owned(), json!(chains));
    }
    if let Some(fallback) = &patch.fallback {
        let mut value = Map::new();
        if let Some(on) = &fallback.on {
            value.insert("on".to_owned(), json!(on));
        }
        if let Some(max_hops) = fallback.max_hops {
            value.insert("max_hops".to_owned(), json!(max_hops));
        }
        object.insert("fallback".to_owned(), Value::Object(value));
    }
    if let Some(notes) = &patch.notes {
        object.insert("notes".to_owned(), json!(notes));
    }
    if let Some(agent_notes) = &patch.agent_notes {
        object.insert("agent_notes".to_owned(), json!(agent_notes));
    }
    if let Some(hidden) = &patch.hidden {
        object.insert("hidden".to_owned(), json!(hidden));
    }
    if let Some(favorites) = &patch.favorites {
        object.insert("favorites".to_owned(), json!(favorites));
    }
    if let Some(pricing) = &patch.pricing {
        let mut value = Map::new();
        for (reference, entry) in pricing {
            let mut item = Map::new();
            item.insert("input".to_owned(), json!(entry.input));
            item.insert("output".to_owned(), json!(entry.output));
            if let Some(read) = entry.cache_read {
                item.insert("cacheRead".to_owned(), json!(read));
            }
            if let Some(write) = entry.cache_write {
                item.insert("cacheWrite".to_owned(), json!(write));
            }
            value.insert(reference.clone(), Value::Object(item));
        }
        object.insert("pricing".to_owned(), Value::Object(value));
    }
    if let Some(quota) = &patch.quota {
        let mut value = Map::new();
        if let Some(reserve) = quota.reserve_percent {
            value.insert("reserve_percent".to_owned(), json!(reserve));
        }
        if let Some(window) = quota.window_hours {
            value.insert("window_hours".to_owned(), json!(window));
        }
        if let Some(rule) = &quota.default {
            value.insert("default".to_owned(), rule_value(rule));
        }
        if let Some(models) = &quota.models {
            let mut map = Map::new();
            for (reference, rule) in models {
                map.insert(reference.clone(), rule_value(rule));
            }
            value.insert("models".to_owned(), Value::Object(map));
        }
        object.insert("quota".to_owned(), Value::Object(value));
    }
    Value::Object(object)
}

fn rule_value(rule: &QuotaRule) -> Value {
    let mut map = Map::new();
    if let Some(cost) = rule.max_cost_usd {
        map.insert("max_cost_usd".to_owned(), json!(cost));
    }
    if let Some(requests) = rule.max_requests {
        map.insert("max_requests".to_owned(), json!(requests));
    }
    Value::Object(map)
}

/// Recursive merge: objects merge key-by-key, everything else replaces, `null`
/// removes a key.
fn deep_merge(base: &Value, patch: &Value) -> Value {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            let mut merged = base.clone();
            for (key, value) in patch {
                if value.is_null() {
                    merged.remove(key);
                } else if let Some(current) = merged.get(key) {
                    merged.insert(key.clone(), deep_merge(current, value));
                } else {
                    merged.insert(key.clone(), value.clone());
                }
            }
            Value::Object(merged)
        }
        _ => patch.clone(),
    }
}
