//! Provider rate-limit header tracking and live quota/balance probing.

mod headers;
mod probes;
mod time;
mod types;
mod util;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::join_all;
use reqwest::Client;

pub use headers::{ProviderRateLimit, extract_rate_limit};
pub use time::{parse_reset_timestamp, parse_rfc3339_ms};
pub use types::{
    ProviderApiSpend, ProviderBalanceSummary, ProviderQuotaProbeResult, ProviderQuotaTarget,
    QuotaCapability, QuotaWindow, provider_quota_capability,
};

use probes::{
    probe_deepseek, probe_minimax, probe_moonshot, probe_opencode_go, probe_openrouter,
    probe_siliconflow, probe_zai,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

/// Build a default HTTP client for quota probes when no shared `LlmClient` pool is attached.
#[must_use]
pub fn default_http_client() -> Client {
    Client::builder()
        .timeout(PROBE_TIMEOUT)
        .user_agent(concat!("opencode/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Current Unix epoch timestamp in milliseconds.
#[must_use]
pub fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Probe multiple providers concurrently.
pub async fn fetch_provider_quotas(
    http: &Client,
    targets: &[ProviderQuotaTarget],
) -> Vec<ProviderQuotaProbeResult> {
    join_all(
        targets
            .iter()
            .map(|target| fetch_provider_quota(http, target)),
    )
    .await
}

/// Probe a single provider's live balance/quota API.
pub async fn fetch_provider_quota(
    http: &Client,
    target: &ProviderQuotaTarget,
) -> ProviderQuotaProbeResult {
    let now_ms = now_epoch_ms();
    match target.provider_id.as_str() {
        "openrouter" => probe_openrouter(http, target, now_ms).await,
        "deepseek" => probe_deepseek(http, target, now_ms).await,
        "moonshotai" | "moonshotai-cn" => probe_moonshot(http, target, now_ms).await,
        "siliconflow" | "siliconflow-cn" => probe_siliconflow(http, target, now_ms).await,
        "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" => {
            probe_zai(http, target, now_ms).await
        }
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => {
            probe_minimax(http, target, now_ms).await
        }
        "opencode-go" => probe_opencode_go(http, target, now_ms).await,
        other => ProviderQuotaProbeResult::unsupported(other, now_ms),
    }
}
