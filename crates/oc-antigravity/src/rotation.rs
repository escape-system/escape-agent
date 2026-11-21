//! Rate-limit reason classification, retry backoff calculation, Health Score tracking,
//! Token Bucket throttling, and hybrid multi-account scoring.

use crate::config::{HealthScoreConfig, TokenBucketConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Normalized reason why a Cloud Code Assist attempt failed or hit a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitReason {
    /// Upstream `QUOTA_EXHAUSTED` (quota window depleted, longer cooldown).
    QuotaExhausted,
    /// Short-burst `RATE_LIMIT_EXCEEDED` / RPM ceiling.
    RateLimitExceeded,
    /// `MODEL_CAPACITY_EXHAUSTED` (short transient saturation).
    ModelCapacityExhausted,
    /// Upstream 5xx server overload.
    ServerError,
    /// Unclassified 429 or transient error.
    Unknown,
}

impl RateLimitReason {
    /// Base minimum backoff in milliseconds when upstream omits a retry hint.
    #[must_use]
    pub const fn min_delay_ms(self) -> i64 {
        match self {
            Self::QuotaExhausted => 60_000,
            Self::RateLimitExceeded | Self::Unknown => 30_000,
            Self::ModelCapacityExhausted => 15_000,
            Self::ServerError => 10_000,
        }
    }
}

/// Classify a Cloud Code Assist error status + response body into [`RateLimitReason`].
#[must_use]
pub fn parse_rate_limit_reason(status: u16, body: &str) -> RateLimitReason {
    if status >= 500 {
        tracing::debug!(
            status,
            reason = "server_error",
            "antigravity: rate-limit reason classified"
        );
        return RateLimitReason::ServerError;
    }
    let upper = body.to_ascii_uppercase();
    // Quota-family reasons win over the generic rate-limit arm: a 429 whose body says
    // `Resource has been exhausted (e.g. check quota)` is a depleted quota window, which
    // needs the longer cooldown plus a family-wide base-key cooldown, not a burst ceiling.
    // `MODEL_CAPACITY_EXHAUSTED` is treated the same way (it is an account-wide quota stop
    // upstream, not a per-model RPM ceiling).
    if upper.contains("RESOURCE_EXHAUSTED")
        || upper.contains("RESOURCE EXHAUSTED")
        || upper.contains("RESOURCE HAS BEEN EXHAUSTED")
        || upper.contains("QUOTA_EXHAUSTED")
        || upper.contains("QUOTA EXHAUSTED")
        || upper.contains("INSUFFICIENT_QUOTA")
        || upper.contains("MODEL_CAPACITY_EXHAUSTED")
        || upper.contains("DAILY LIMIT")
        || upper.contains("CHECK QUOTA")
    {
        return RateLimitReason::QuotaExhausted;
    }
    if upper.contains("CAPACITY_EXHAUSTED") || upper.contains("OVERLOADED") {
        return RateLimitReason::ModelCapacityExhausted;
    }
    if upper.contains("RATE_LIMIT_EXCEEDED")
        || upper.contains("RATE LIMIT")
        || upper.contains("TOO_MANY_REQUESTS")
        || upper.contains("TOO MANY REQUESTS")
        || status == 429
    {
        return RateLimitReason::RateLimitExceeded;
    }
    tracing::debug!(
        status,
        reason = ?RateLimitReason::Unknown,
        "antigravity: rate-limit reason classified"
    );
    RateLimitReason::Unknown
}

/// Parse Google RPC / natural-language duration strings (`"1h2m3s"`, `"500ms"`, `"2.5s"`)
/// into milliseconds.
#[must_use]
pub fn parse_duration_ms(input: &str) -> Option<i64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }

    // Fast path: plain numeric seconds (e.g. from Retry-After: "30").
    if s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && let Ok(secs) = s.parse::<f64>()
        && secs.is_finite()
        && secs >= 0.0
    {
        return Some(saturating_f64_to_i64(secs * 1000.0));
    }

    let mut total_ms: f64 = 0.0;
    let mut matched_any = false;
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if !(bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
            continue;
        }
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let Ok(num) = s[num_start..i].parse::<f64>() else {
            return None;
        };
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = s[unit_start..i].to_ascii_lowercase();
        let multiplier = match unit.as_str() {
            "ms" | "millisecond" | "milliseconds" => 1.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            _ => return None,
        };
        total_ms += num * multiplier;
        matched_any = true;
    }

    if matched_any && total_ms.is_finite() && total_ms >= 0.0 {
        Some(saturating_f64_to_i64(total_ms))
    } else {
        None
    }
}

/// Extract upstream retry delay (milliseconds) from Google Cloud Code JSON `details`,
/// error messages, or HTTP `Retry-After` / `retry-after-ms` headers.
#[must_use]
pub fn extract_retry_info_from_body(
    body: &str,
    retry_after_header: Option<&str>,
    retry_after_ms_header: Option<&str>,
) -> Option<i64> {
    if let Some(ms_hdr) = retry_after_ms_header
        && let Ok(ms) = ms_hdr.trim().parse::<i64>()
        && ms >= 0
    {
        return Some(ms);
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(ms) = find_retry_delay_in_json(&json) {
            return Some(ms);
        }
        if let Some(msg) = json
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            && let Some(ms) = extract_duration_from_message(msg)
        {
            return Some(ms);
        }
    }

    if let Some(ms) = extract_duration_from_message(body) {
        return Some(ms);
    }

    if let Some(hdr) = retry_after_header
        && let Some(ms) = parse_duration_ms(hdr)
    {
        return Some(ms);
    }

    None
}

fn find_retry_delay_in_json(val: &serde_json::Value) -> Option<i64> {
    match val {
        serde_json::Value::Object(map) => {
            if let Some(delay) = map.get("retryDelay").and_then(serde_json::Value::as_str)
                && let Some(ms) = parse_duration_ms(delay)
            {
                return Some(ms);
            }
            if let Some(delay) = map
                .get("quotaResetDelay")
                .and_then(serde_json::Value::as_str)
                && let Some(ms) = parse_duration_ms(delay)
            {
                return Some(ms);
            }
            for child in map.values() {
                if let Some(ms) = find_retry_delay_in_json(child) {
                    return Some(ms);
                }
            }
            None
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(ms) = find_retry_delay_in_json(item) {
                    return Some(ms);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_duration_from_message(msg: &str) -> Option<i64> {
    let lower = msg.to_ascii_lowercase();
    for marker in [
        "reset after ",
        "try again in ",
        "retry in ",
        "retry after ",
        "wait ",
    ] {
        if let Some(idx) = lower.find(marker) {
            let tail = &lower[idx + marker.len()..];
            let token: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == ' ')
                .collect();
            let trimmed = token.trim();
            // Stop cleanly before trailing non-duration words.
            let candidate = trimmed
                .split(" before ")
                .next()
                .unwrap_or(trimmed)
                .trim_end_matches('.');
            if let Some(ms) = parse_duration_ms(candidate) {
                return Some(ms);
            }
        }
    }
    None
}

/// Compute backoff delay in milliseconds honoring upstream hints and capped exponential growth.
#[must_use]
pub fn calculate_backoff_ms(
    reason: RateLimitReason,
    consecutive_failures: u32,
    retry_after_ms: Option<i64>,
    default_retry_ms: i64,
    max_backoff_ms: i64,
) -> i64 {
    let base = reason.min_delay_ms().max(default_retry_ms.max(1000));
    let shift = consecutive_failures.saturating_sub(1).min(10);
    let exp = base.saturating_mul(1_i64 << shift);
    let hinted = retry_after_ms.unwrap_or(0).max(0);
    let max_cap = max_backoff_ms.max(base).max(hinted);
    exp.max(hinted).min(max_cap)
}

/// Per-account passive-recovery health score tracker.
#[derive(Debug, Clone)]
pub struct HealthScoreTracker {
    cfg: HealthScoreConfig,
    scores: HashMap<String, AccountHealth>,
}

#[derive(Debug, Clone, Copy)]
struct AccountHealth {
    score: f64,
    last_updated_ms: i64,
    consecutive_failures: u32,
}

impl HealthScoreTracker {
    /// Construct a tracker using [`HealthScoreConfig`].
    #[must_use]
    pub fn new(cfg: HealthScoreConfig) -> Self {
        Self {
            cfg,
            scores: HashMap::new(),
        }
    }

    /// Current recovered score for `key` at `now_ms`.
    #[allow(clippy::cast_precision_loss)]
    pub fn score(&mut self, key: &str, now_ms: i64) -> f64 {
        let entry = self.scores.entry(key.to_owned()).or_insert(AccountHealth {
            score: f64::from(self.cfg.initial),
            last_updated_ms: now_ms,
            consecutive_failures: 0,
        });
        let elapsed_hours = ((now_ms - entry.last_updated_ms).max(0) as f64) / 3_600_000.0;
        let recovered = elapsed_hours * f64::from(self.cfg.recovery_rate_per_hour);
        entry.score = (entry.score + recovered).clamp(0.0, f64::from(self.cfg.max_score));
        entry.last_updated_ms = now_ms;
        entry.score
    }

    /// Reward successful completion.
    pub fn record_success(&mut self, key: &str, now_ms: i64) {
        let current = self.score(key, now_ms);
        if let Some(entry) = self.scores.get_mut(key) {
            entry.score = (current + f64::from(self.cfg.success_reward))
                .clamp(0.0, f64::from(self.cfg.max_score));
            entry.consecutive_failures = 0;
            entry.last_updated_ms = now_ms;
        }
    }

    /// Penalize rate limit response and increment consecutive failures.
    pub fn record_rate_limit(&mut self, key: &str, now_ms: i64) -> u32 {
        let current = self.score(key, now_ms);
        let penalty = self.cfg.rate_limit_penalty;
        if let Some(entry) = self.scores.get_mut(key) {
            let delta = if penalty <= 0 { penalty } else { -penalty };
            entry.score = (current + f64::from(delta)).clamp(0.0, f64::from(self.cfg.max_score));
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.last_updated_ms = now_ms;
            return entry.consecutive_failures;
        }
        1
    }

    /// Penalize non-429 failure.
    pub fn record_failure(&mut self, key: &str, now_ms: i64) -> u32 {
        let current = self.score(key, now_ms);
        let penalty = self.cfg.failure_penalty;
        if let Some(entry) = self.scores.get_mut(key) {
            let delta = if penalty <= 0 { penalty } else { -penalty };
            entry.score = (current + f64::from(delta)).clamp(0.0, f64::from(self.cfg.max_score));
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            entry.last_updated_ms = now_ms;
            return entry.consecutive_failures;
        }
        1
    }

    /// Number of consecutive failures recorded on `key`.
    #[must_use]
    pub fn consecutive_failures(&self, key: &str) -> u32 {
        self.scores.get(key).map_or(0, |e| e.consecutive_failures)
    }

    /// Return `true` when score meets or exceeds `min_usable`.
    pub fn is_usable(&mut self, key: &str, now_ms: i64) -> bool {
        self.score(key, now_ms) >= f64::from(self.cfg.min_usable)
    }
}

/// Per-account client-side Token Bucket throttling tracker.
#[derive(Debug, Clone)]
pub struct TokenBucketTracker {
    cfg: TokenBucketConfig,
    buckets: HashMap<String, BucketState>,
}

#[derive(Debug, Clone, Copy)]
struct BucketState {
    tokens: f64,
    last_refill_ms: i64,
}

impl TokenBucketTracker {
    /// Construct a token bucket tracker using [`TokenBucketConfig`].
    #[must_use]
    pub fn new(cfg: TokenBucketConfig) -> Self {
        Self {
            cfg,
            buckets: HashMap::new(),
        }
    }

    /// Available tokens for `key` after passive refill up to `now_ms`.
    #[allow(clippy::cast_precision_loss)]
    pub fn available(&mut self, key: &str, now_ms: i64) -> f64 {
        let max_tokens = f64::from(self.cfg.max_tokens);
        let initial = f64::from(self.cfg.initial_tokens).min(max_tokens);
        let entry = self.buckets.entry(key.to_owned()).or_insert(BucketState {
            tokens: initial,
            last_refill_ms: now_ms,
        });
        let elapsed_min = ((now_ms - entry.last_refill_ms).max(0) as f64) / 60_000.0;
        let refilled = elapsed_min * self.cfg.regeneration_rate_per_minute;
        entry.tokens = (entry.tokens + refilled).min(max_tokens);
        entry.last_refill_ms = now_ms;
        entry.tokens
    }

    /// Consume 1 token if available; returns `true` if a token was consumed.
    pub fn try_consume(&mut self, key: &str, now_ms: i64) -> bool {
        let avail = self.available(key, now_ms);
        if avail < 1.0 {
            return false;
        }
        if let Some(entry) = self.buckets.get_mut(key) {
            entry.tokens = (avail - 1.0).max(0.0);
        }
        true
    }

    /// Refund 1 token when an upstream attempt never consumed quota (e.g. transport error).
    pub fn refund(&mut self, key: &str, now_ms: i64) {
        let avail = self.available(key, now_ms);
        let max_tokens = f64::from(self.cfg.max_tokens);
        if let Some(entry) = self.buckets.get_mut(key) {
            entry.tokens = (avail + 1.0).min(max_tokens);
        }
    }
}

/// Candidate input for hybrid account scoring.
#[derive(Debug, Clone, Copy)]
pub struct HybridCandidate<'a> {
    /// Candidate pool index.
    pub index: usize,
    /// Stable account tracking key (`token_hash`).
    pub key: &'a str,
    /// Timestamp when this account was last used (epoch ms).
    pub last_used_ms: i64,
    /// Whether this candidate is the currently sticky index.
    pub is_sticky: bool,
}

/// Select the highest-scoring eligible candidate using Health Score + Token Bucket + LRU.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn select_hybrid_account(
    candidates: &[HybridCandidate<'_>],
    health: &mut HealthScoreTracker,
    buckets: &mut TokenBucketTracker,
    max_health: i32,
    max_tokens: u32,
    now_ms: i64,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }

    let max_h = f64::from(max_health.max(1));
    let max_t = f64::from(max_tokens.max(1));

    let mut best_idx = None;
    let mut best_score = f64::NEG_INFINITY;

    for cand in candidates {
        let h_norm = health.score(cand.key, now_ms) / max_h;
        let t_norm = buckets.available(cand.key, now_ms) / max_t;
        let idle_hours = ((now_ms - cand.last_used_ms).max(0) as f64) / 3_600_000.0;
        let lru_bonus = (idle_hours / 24.0).min(1.0);
        let sticky_bonus = if cand.is_sticky { 0.05 } else { 0.0 };
        let token_gate_penalty = if buckets.available(cand.key, now_ms) < 1.0 {
            -1.0
        } else {
            0.0
        };

        let composite = (h_norm * 0.55)
            + (t_norm * 0.30)
            + (lru_bonus * 0.15)
            + sticky_bonus
            + token_gate_penalty;
        if composite > best_score {
            best_score = composite;
            best_idx = Some(cand.index);
        }
    }

    best_idx
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn saturating_f64_to_i64(val: f64) -> i64 {
    if val >= i64::MAX as f64 {
        i64::MAX
    } else if val <= 0.0 {
        0
    } else {
        val.round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rpc_and_human_durations() {
        assert_eq!(parse_duration_ms("500ms"), Some(500));
        assert_eq!(parse_duration_ms("2.5s"), Some(2500));
        assert_eq!(parse_duration_ms("1h2m3s"), Some(3_723_000));
        assert_eq!(parse_duration_ms("45"), Some(45_000));
    }

    #[test]
    fn extracts_retry_delay_from_google_error_json() {
        let body = r#"{
            "error": {
                "code": 429,
                "message": "Your quota will reset after 2m30s.",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.RetryInfo",
                    "retryDelay": "42s"
                }]
            }
        }"#;
        assert_eq!(extract_retry_info_from_body(body, None, None), Some(42_000));
        assert_eq!(
            parse_rate_limit_reason(429, "MODEL_CAPACITY_EXHAUSTED"),
            RateLimitReason::QuotaExhausted
        );
    }

    #[test]
    fn resource_exhausted_is_quota() {
        // The exact in-band sentence Antigravity returns inside an HTTP 200 stream.
        assert_eq!(
            parse_rate_limit_reason(429, "Resource has been exhausted (e.g. check quota)."),
            RateLimitReason::QuotaExhausted
        );
        assert_eq!(
            parse_rate_limit_reason(429, "RESOURCE_EXHAUSTED"),
            RateLimitReason::QuotaExhausted
        );
        assert_eq!(
            parse_rate_limit_reason(429, "MODEL_CAPACITY_EXHAUSTED"),
            RateLimitReason::QuotaExhausted
        );
        // Status 200 keeps working: in-band frames arrive on OK responses.
        assert_eq!(
            parse_rate_limit_reason(200, "Resource has been exhausted (e.g. check quota)."),
            RateLimitReason::QuotaExhausted
        );
        // Burst ceilings are NOT demoted to quota.
        assert_eq!(
            parse_rate_limit_reason(429, "too many requests"),
            RateLimitReason::RateLimitExceeded
        );
        assert_eq!(
            parse_rate_limit_reason(429, "RATE_LIMIT_EXCEEDED"),
            RateLimitReason::RateLimitExceeded
        );
    }

    #[test]
    fn health_and_token_bucket_score_prefers_healthier_account() {
        let mut health = HealthScoreTracker::new(HealthScoreConfig::default());
        let mut buckets = TokenBucketTracker::new(TokenBucketConfig::default());
        let now = 1_700_000_000_000;

        health.record_rate_limit("acct-a", now);
        health.record_rate_limit("acct-a", now);
        health.record_success("acct-b", now);

        let picked = select_hybrid_account(
            &[
                HybridCandidate {
                    index: 0,
                    key: "acct-a",
                    last_used_ms: now - 10_000,
                    is_sticky: true,
                },
                HybridCandidate {
                    index: 1,
                    key: "acct-b",
                    last_used_ms: now - 10_000,
                    is_sticky: false,
                },
            ],
            &mut health,
            &mut buckets,
            100,
            50,
            now,
        );
        assert_eq!(picked, Some(1));
    }
}
