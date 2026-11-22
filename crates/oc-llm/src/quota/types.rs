use serde::{Deserialize, Serialize};

/// How a provider can surface quota/usage data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaCapability {
    /// Provider exposes an authenticated quota/balance/limits HTTP endpoint.
    LiveApi,
    /// Provider does not expose a balance endpoint; live HTTP rate-limit headers
    /// and local usage statistics are reported instead.
    RateLimitHeadersOnly,
}

/// Returns the quota capability tier and an optional honest capability note for `provider_id`.
#[must_use]
pub fn provider_quota_capability(provider_id: &str) -> (QuotaCapability, Option<&'static str>) {
    match provider_id {
        "deepseek" | "moonshotai" | "moonshotai-cn" | "siliconflow" | "siliconflow-cn" => {
            (QuotaCapability::LiveApi, None)
        }
        "openrouter" => (
            QuotaCapability::LiveApi,
            Some(
                "Fetches OpenRouter key limits and usage (/api/v1/key). Add an OpenRouter Management Key to also fetch total wallet credits (/api/v1/credits).",
            ),
        ),
        "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" => (
            QuotaCapability::LiveApi,
            Some(
                "Queries the Z.AI / BigModel Coding Plan quota monitor; standard pay-as-you-go keys may report zero plan windows.",
            ),
        ),
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => (
            QuotaCapability::LiveApi,
            Some(
                "Queries MiniMax Token Plan remaining quota; standard pay-as-you-go keys or non-subscription endpoints may require a Token Plan key or session.",
            ),
        ),
        "opencode-go" => (
            QuotaCapability::LiveApi,
            Some("Live usage via the OpenCode Go usage API (rolling 5h / weekly / monthly)"),
        ),
        "opencode" => (
            QuotaCapability::RateLimitHeadersOnly,
            Some(
                "OpenCode Go rolling (5h), weekly, and monthly % limits are served via OpenCode Console web auth rather than a public API-key endpoint. Showing live response headers and local tracked usage.",
            ),
        ),
        "github-copilot" => (
            QuotaCapability::RateLimitHeadersOnly,
            Some(
                "GitHub Copilot uses seat subscription billing without a balance API endpoint. Showing captured rate-limit headers and local tracked usage.",
            ),
        ),
        // Antigravity quota is served by oc-server's own aggregator for the
        // `google` provider, not by a probe in this module.
        "anthropic" | "openai" | "google" | "groq" | "xai" | "fireworks-ai" | "togetherai" => (
            QuotaCapability::RateLimitHeadersOnly,
            Some(
                "Provider does not expose a public remaining-balance endpoint for standard API keys. Showing live rate-limit headers captured from requests and local tracked usage.",
            ),
        ),
        "nous" => (
            QuotaCapability::RateLimitHeadersOnly,
            Some(
                "Nous Research inference keys do not expose a wallet balance endpoint (manage credits at portal.nousresearch.com). Showing captured rate-limit headers and local tracked usage.",
            ),
        ),
        _ => (
            QuotaCapability::RateLimitHeadersOnly,
            Some(
                "No balance API adapter for this provider. Showing captured rate-limit headers and local tracked usage.",
            ),
        ),
    }
}

/// Target descriptor used to query a provider's live quota/balance endpoint.
#[derive(Debug, Clone)]
pub struct ProviderQuotaTarget {
    pub provider_id: String,
    pub api_key: String,
    pub management_key: Option<String>,
    pub base_url: Option<String>,
}

/// One metered quota or allowance window reported by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaWindow {
    pub id: String,
    pub label: String,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_label: Option<String>,
}

/// Currency wallet/credit balance reported by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderBalanceSummary {
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topped_up: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_available: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_free_tier: Option<bool>,
}

/// Key/account spend breakdown returned directly by a provider API (e.g. `OpenRouter`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderApiSpend {
    pub currency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<f64>,
}

/// Result of querying a provider's live quota/balance endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderQuotaProbeResult {
    pub provider_id: String,
    pub status: String,
    pub fetched_at_ms: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<QuotaWindow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub balances: Vec<ProviderBalanceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_summary: Option<ProviderApiSpend>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProviderQuotaProbeResult {
    pub(super) fn unsupported(provider_id: &str, now_ms: i64) -> Self {
        Self {
            provider_id: provider_id.to_owned(),
            status: "unsupported".to_owned(),
            fetched_at_ms: now_ms,
            windows: Vec::new(),
            balances: Vec::new(),
            rate_limit_rule: None,
            key_label: None,
            spend_summary: None,
            warnings: Vec::new(),
            error: None,
        }
    }

    pub(super) fn error_result(provider_id: &str, now_ms: i64, message: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.to_owned(),
            status: "error".to_owned(),
            fetched_at_ms: now_ms,
            windows: Vec::new(),
            balances: Vec::new(),
            rate_limit_rule: None,
            key_label: None,
            spend_summary: None,
            warnings: Vec::new(),
            error: Some(message.into()),
        }
    }
}
