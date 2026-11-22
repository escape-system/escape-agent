use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};

use super::time::parse_reset_timestamp;
use crate::retry::parse_retry_after;

/// Rate-limit headers captured from a live HTTP response for a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRateLimit {
    pub provider_id: String,
    pub model_id: String,
    pub observed_at_ms: i64,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_reset_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_reset_raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_remaining: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_reset_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_reset_raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

/// Extract normalized rate-limit telemetry from provider HTTP response headers.
#[must_use]
pub fn extract_rate_limit(
    provider_id: &str,
    model_id: &str,
    status: u16,
    headers: &HeaderMap,
    now_ms: i64,
) -> Option<ProviderRateLimit> {
    let requests_limit = header_u64(
        headers,
        &[
            "x-ratelimit-limit-requests",
            "anthropic-ratelimit-requests-limit",
            "x-ratelimit-requests-limit",
            "ratelimit-limit",
        ],
    );
    let requests_remaining = header_u64(
        headers,
        &[
            "x-ratelimit-remaining-requests",
            "anthropic-ratelimit-requests-remaining",
            "x-ratelimit-requests-remaining",
            "ratelimit-remaining",
        ],
    );
    let requests_reset_raw = header_str(
        headers,
        &[
            "x-ratelimit-reset-requests",
            "anthropic-ratelimit-requests-reset",
            "x-ratelimit-requests-reset",
            "ratelimit-reset",
        ],
    );
    let requests_reset_at_ms = requests_reset_raw
        .as_deref()
        .and_then(|raw| parse_reset_timestamp(raw, now_ms));

    let tokens_limit = header_u64(
        headers,
        &[
            "x-ratelimit-limit-tokens",
            "anthropic-ratelimit-tokens-limit",
            "anthropic-ratelimit-input-tokens-limit",
            "x-ratelimit-tokens-limit",
        ],
    );
    let tokens_remaining = header_u64(
        headers,
        &[
            "x-ratelimit-remaining-tokens",
            "anthropic-ratelimit-tokens-remaining",
            "anthropic-ratelimit-input-tokens-remaining",
            "x-ratelimit-tokens-remaining",
        ],
    );
    let tokens_reset_raw = header_str(
        headers,
        &[
            "x-ratelimit-reset-tokens",
            "anthropic-ratelimit-tokens-reset",
            "anthropic-ratelimit-input-tokens-reset",
            "x-ratelimit-tokens-reset",
        ],
    );
    let tokens_reset_at_ms = tokens_reset_raw
        .as_deref()
        .and_then(|raw| parse_reset_timestamp(raw, now_ms));

    let retry_after_ms = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after)
                .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
        });

    let has_any_limit = requests_limit.is_some()
        || requests_remaining.is_some()
        || tokens_limit.is_some()
        || tokens_remaining.is_some()
        || retry_after_ms.is_some()
        || matches!(status, 402 | 429);

    if !has_any_limit {
        return None;
    }

    Some(ProviderRateLimit {
        provider_id: provider_id.to_owned(),
        model_id: model_id.to_owned(),
        observed_at_ms: now_ms,
        status,
        requests_limit,
        requests_remaining,
        requests_reset_at_ms,
        requests_reset_raw,
        tokens_limit,
        tokens_remaining,
        tokens_reset_at_ms,
        tokens_reset_raw,
        retry_after_ms,
    })
}

fn header_str(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn header_u64(headers: &HeaderMap, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    })
}
