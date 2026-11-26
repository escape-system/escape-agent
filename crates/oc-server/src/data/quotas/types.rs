//! Wire DTOs for the unified quota response and the local usage rollup.
//!
//! Every shape here is read by the settings UI, so the serde attributes are
//! part of the contract; the facade owns the handlers that serialize them.

use std::collections::BTreeMap;

use oc_antigravity::AccountStatusSummary;
use oc_llm::{
    ProviderApiSpend, ProviderBalanceSummary, ProviderQuotaProbeResult, ProviderRateLimit,
    QuotaCapability, QuotaWindow,
};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UnifiedQuotasResponse {
    pub(super) fetched_at_ms: i64,
    pub(super) summary: UnifiedQuotaSummary,
    pub(super) providers: Vec<ProviderQuotaEntry>,
    /// True when this payload came from a missing or stale cache and the server
    /// started (or joined) a background probe behind it. Omitted when false,
    /// never `null`, so a client that does not know the field keeps working.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) refreshing: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UnifiedQuotaSummary {
    pub(super) provider_count: usize,
    pub(super) live_quota_count: usize,
    pub(super) spend5h_usd: f64,
    pub(super) spend24h_usd: f64,
    pub(super) spend7d_usd: f64,
    pub(super) spend30d_usd: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProviderQuotaEntry {
    #[serde(rename = "providerID")]
    pub(super) provider_id: String,
    pub(super) name: String,
    pub(super) connected: bool,
    pub(super) connection_source: String,
    pub(super) capability: QuotaCapability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capability_note: Option<String>,
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) fetched_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) has_management_key: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) management_key_source: Option<ManagementKeySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) key_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rate_limit_rule: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) windows: Vec<QuotaWindowEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) balances: Vec<ProviderBalanceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) spend_summary: Option<ProviderApiSpend>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rate_limit: Option<ProviderRateLimit>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) accounts: Vec<AccountStatusSummary>,
    /// Stored credentials of this provider in rotation order (active first),
    /// each joined with the runtime's per-key cooldown state. Omitted when the
    /// provider owns no credential row, never null.
    #[serde(rename = "keys", skip_serializing_if = "Vec::is_empty")]
    pub(super) keys: Vec<ProviderKeyEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) local_usage: Option<ProviderLocalUsage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// Normalize a probe result's wire `status` string.
pub(super) fn probe_status(probe: &ProviderQuotaProbeResult) -> String {
    if probe.error.is_some() {
        "error".to_owned()
    } else if probe.status == "ok" && !probe.warnings.is_empty() {
        "partial".to_owned()
    } else {
        probe.status.clone()
    }
}

/// All-time `(cost_usd, requests)` per `(provider_id, credential_id)`.
pub(super) type KeyUsageMap = BTreeMap<(String, String), (f64, u64)>;

/// Cached local usage snapshot covering both provider windows and per-key totals.
#[derive(Debug, Clone, Default)]
pub(super) struct UsageSnapshot {
    pub(super) providers: BTreeMap<String, ProviderLocalUsage>,
    pub(super) keys: KeyUsageMap,
}

/// One stored credential of a provider, joined with its rotation state, live
/// per-key quota probe (for `LiveApi` providers), and attributed local usage.
///
/// `limited`/`cooldown_until_ms`/`kind`/`failures` come from the runtime-owned
/// [`oc_llm::KeyPool`]; `active` is the stored selection from `oc_store`. A
/// provider with several credentials lists them in rotation order, so `keys[0]`
/// is the key a request would try first.
///
/// `used_cost_usd` / `used_requests` reflect assistant messages carrying this
/// `credentialID` in `model.credentialID`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProviderKeyEntry {
    #[serde(rename = "credentialID")]
    pub(super) credential_id: String,
    pub(super) label: String,
    pub(super) active: bool,
    pub(super) limited: bool,
    #[serde(rename = "cooldownUntilMs", skip_serializing_if = "Option::is_none")]
    pub(super) cooldown_until_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failures: Option<u32>,
    #[serde(rename = "usedCostUsd")]
    pub(super) used_cost_usd: f64,
    #[serde(rename = "usedRequests")]
    pub(super) used_requests: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<String>,
    #[serde(rename = "fetchedAtMs", skip_serializing_if = "Option::is_none")]
    pub(super) fetched_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cached: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) windows: Vec<QuotaWindowEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) balances: Vec<ProviderBalanceSummary>,
    #[serde(rename = "spendSummary", skip_serializing_if = "Option::is_none")]
    pub(super) spend_summary: Option<ProviderApiSpend>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

/// Where the `OpenRouter` management key the probe uses comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ManagementKeySource {
    /// `OPENROUTER_MANAGEMENT_KEY` owns the key; stored state cannot win.
    Env,
    /// Stored as a credential (metadata or the synthetic management credential).
    Credential,
    /// Supplied by `providers.openrouter.settings.managementKey`.
    Config,
}

/// A [`QuotaWindow`] plus the stable `groupKey` a client localizes on.
///
/// `label` stays on the wire as the server-side English fallback.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QuotaWindowEntry {
    pub(super) id: String,
    pub(super) label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) group_key: Option<String>,
    pub(super) unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) remaining: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) used_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) remaining_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) limited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reset_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reset_label: Option<String>,
}

impl QuotaWindowEntry {
    /// Project a probe window; provider APIs have no pool grouping.
    pub(super) fn from_probe(window: &QuotaWindow) -> Self {
        Self {
            id: window.id.clone(),
            label: window.label.clone(),
            group_key: None,
            unit: window.unit.clone(),
            limit: window.limit,
            used: window.used,
            remaining: window.remaining,
            used_percent: window.used_percent,
            remaining_percent: window.remaining_percent,
            limited: window.limited,
            reset_at_ms: window.reset_at_ms,
            reset_label: window.reset_label.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsageWindowTotals {
    pub(super) steps: i64,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
    pub(super) total_tokens: i64,
    pub(super) cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelUsageRow {
    #[serde(rename = "modelID")]
    pub(super) model_id: String,
    pub(super) steps: i64,
    pub(super) total_tokens: i64,
    pub(super) cost_usd: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProviderLocalUsage {
    pub(super) five_hours: UsageWindowTotals,
    pub(super) day: UsageWindowTotals,
    pub(super) week: UsageWindowTotals,
    pub(super) month: UsageWindowTotals,
    pub(super) all_time: UsageWindowTotals,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) top_models: Vec<ModelUsageRow>,
}

#[derive(Default)]
pub(super) struct ProviderUsageAccum {
    pub(super) five_hours: UsageWindowTotals,
    pub(super) day: UsageWindowTotals,
    pub(super) week: UsageWindowTotals,
    pub(super) month: UsageWindowTotals,
    pub(super) all_time: UsageWindowTotals,
    pub(super) models: BTreeMap<String, (i64, i64, f64)>,
}
