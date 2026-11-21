//! Antigravity configuration discovery, JSONC loading, clamping, and environment overrides.

use crate::error::{AntigravityError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Strategy for selecting among available Antigravity accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountSelectionStrategy {
    /// Stay on the current account until rate-limited or unhealthy.
    Sticky,
    /// Cycle through eligible accounts on every request.
    RoundRobin,
    /// Score-driven selection combining Health Score, Token Bucket, and LRU.
    #[default]
    Hybrid,
}

/// Rate-limit scheduling policy across accounts and waiting windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchedulingMode {
    /// Preserve prompt cache affinity by waiting briefly for the sticky account.
    #[default]
    CacheFirst,
    /// Rotate to another eligible account immediately on any rate limit.
    Balance,
    /// Prioritize fastest immediate execution using token-bucket + health availability.
    PerformanceFirst,
}

/// Search thinking mode for `google_search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchMode {
    /// Choose based on input hints or default to fast.
    #[default]
    Auto,
    /// Lower thinking budget for quick factual queries.
    Fast,
    /// Higher thinking budget for multi-step synthesis.
    Deep,
}

/// Configuration for thought signature caching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SignatureCacheConfig {
    /// Enable signature caching.
    pub enabled: bool,
    /// In-memory TTL in seconds.
    pub memory_ttl_seconds: u64,
    /// On-disk persistence TTL in seconds.
    pub disk_ttl_seconds: u64,
    /// Minimum flush cadence in seconds.
    pub write_interval_seconds: u64,
}

impl Default for SignatureCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_ttl_seconds: 3600,
            disk_ttl_seconds: 172_800,
            write_interval_seconds: 60,
        }
    }
}

/// Configuration for the `google_search` grounding tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    /// Default search depth mode.
    pub default_mode: SearchMode,
    /// Optional cap on citations rendered in output.
    pub max_results: Option<u32>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            default_mode: SearchMode::Auto,
            max_results: None,
        }
    }
}

/// Configuration for per-account Health Score tracking in hybrid rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthScoreConfig {
    /// Starting health score for new accounts.
    pub initial: i32,
    /// Points added after a successful request.
    pub success_reward: i32,
    /// Score delta on HTTP 429 rate-limit responses (typically negative).
    pub rate_limit_penalty: i32,
    /// Score delta on non-429 failures (typically negative).
    pub failure_penalty: i32,
    /// Passive points recovered per elapsed hour.
    pub recovery_rate_per_hour: i32,
    /// Minimum score before an account is skipped while healthier accounts exist.
    pub min_usable: i32,
    /// Maximum possible health score.
    pub max_score: i32,
}

impl Default for HealthScoreConfig {
    fn default() -> Self {
        Self {
            initial: 70,
            success_reward: 1,
            rate_limit_penalty: -10,
            failure_penalty: -20,
            recovery_rate_per_hour: 2,
            min_usable: 50,
            max_score: 100,
        }
    }
}

/// Configuration for per-account Token Bucket throttling in hybrid rotation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenBucketConfig {
    /// Maximum tokens held per account.
    pub max_tokens: u32,
    /// Token refill rate per minute.
    pub regeneration_rate_per_minute: f64,
    /// Starting token balance for newly tracked accounts.
    pub initial_tokens: u32,
}

impl Default for TokenBucketConfig {
    fn default() -> Self {
        Self {
            max_tokens: 50,
            regeneration_rate_per_minute: 6.0,
            initial_tokens: 50,
        }
    }
}

/// Complete configuration for `oc-antigravity`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
#[serde(default)]
pub struct AntigravityConfig {
    /// Preserve signed thinking blocks instead of stripping them on output.
    pub keep_thinking: bool,
    /// Automatically rewrite corrupted/missing thinking conversations instead of failing hard.
    pub session_recovery: bool,
    /// Thought signature cache tuning.
    pub signature_cache: SignatureCacheConfig,
    /// Multi-account rotation strategy (`sticky`, `round-robin`, `hybrid`).
    pub account_selection_strategy: AccountSelectionStrategy,
    /// Offset initial account cursor using OS process ID.
    pub pid_offset_enabled: bool,
    /// Rotate to another account immediately on first 429 before waiting.
    pub switch_on_first_rate_limit: bool,
    /// Scheduling trade-off mode across accounts.
    pub scheduling_mode: SchedulingMode,
    /// Maximum wait (seconds) in `cache-first` mode when another account is free.
    pub max_cache_first_wait_seconds: u64,
    /// Fallback retry delay (seconds) when upstream omits `Retry-After`.
    pub default_retry_after_seconds: u64,
    /// Maximum exponential backoff delay (seconds).
    pub max_backoff_seconds: u64,
    /// Hard cap (seconds) on cumulative rate-limit waiting within a single turn.
    pub max_rate_limit_wait_seconds: u64,
    /// Optional random jitter ceiling (ms) added before upstream calls.
    pub request_jitter_max_ms: u64,
    /// Allow fallback from Antigravity pool to Gemini CLI pool (and back) when exhausted.
    pub quota_fallback: bool,
    /// Prefer Gemini CLI pool before Antigravity Hub pool for Gemini models.
    pub cli_first: bool,
    /// Google Search grounding tool defaults.
    pub web_search: WebSearchConfig,
    /// Percentage usage (0-100; 100 disables) at which an account enters soft-quota skip.
    pub soft_quota_threshold_percent: u8,
    /// Quota cache freshness window in minutes before soft-quota re-checks live quota.
    pub soft_quota_cache_ttl_minutes: u64,
    /// Background quota refresh interval in minutes (0 disables).
    pub quota_refresh_interval_minutes: u64,
    /// Proactively refresh OAuth tokens before expiry.
    pub proactive_token_refresh: bool,
    /// Seconds before expiry that triggers proactive refresh.
    pub proactive_refresh_buffer_seconds: u64,
    /// Interval between proactive refresh sweeps (seconds).
    pub proactive_refresh_interval_seconds: u64,
    /// Check Antigravity Hub update manifest for runtime User-Agent version alignment.
    pub auto_update: bool,
    /// Retry attempts when Cloud Code returns an empty stream with no candidates.
    pub empty_response_max_attempts: u32,
    /// Base delay for exponential backoff on empty-response retries (milliseconds).
    /// The delay for attempt `n` (0-based) is
    /// `min(base_ms * 2^n, empty_response_max_delay_ms)`.
    pub empty_response_retry_delay_ms: u64,
    /// Cap on a single exponential backoff delay for empty-response retries (milliseconds).
    pub empty_response_max_delay_ms: u64,
    /// Disable injection of the synthetic `skip_thought_signature_validator` marker
    /// into histories that would otherwise be repaired for thinking order. When
    /// `true`, such histories are left untouched; recovery via thinking-stripping
    /// still applies. Set this when an upstream model rejects the sentinel.
    pub disable_synthetic_thought_signature: bool,
    /// Repair mismatched tool call/response IDs in cross-turn histories.
    pub tool_id_recovery: bool,
    /// Harden Claude tool schemas and descriptions to avoid hallucinated arguments.
    pub claude_tool_hardening: bool,
    /// Bind the local OAuth callback listener to `0.0.0.0` instead of `127.0.0.1`.
    pub oauth_server_bind_all: bool,
    /// Health score model parameters.
    pub health_score: HealthScoreConfig,
    /// Token bucket parameters.
    pub token_bucket: TokenBucketConfig,
}

impl Default for AntigravityConfig {
    fn default() -> Self {
        Self {
            keep_thinking: false,
            session_recovery: true,
            signature_cache: SignatureCacheConfig::default(),
            account_selection_strategy: AccountSelectionStrategy::Hybrid,
            pid_offset_enabled: false,
            switch_on_first_rate_limit: true,
            scheduling_mode: SchedulingMode::CacheFirst,
            max_cache_first_wait_seconds: 60,
            default_retry_after_seconds: 60,
            max_backoff_seconds: 60,
            max_rate_limit_wait_seconds: 300,
            request_jitter_max_ms: 0,
            quota_fallback: false,
            cli_first: false,
            web_search: WebSearchConfig::default(),
            soft_quota_threshold_percent: 90,
            soft_quota_cache_ttl_minutes: 10,
            quota_refresh_interval_minutes: 15,
            proactive_token_refresh: true,
            proactive_refresh_buffer_seconds: 1800,
            proactive_refresh_interval_seconds: 300,
            auto_update: true,
            empty_response_max_attempts: 4,
            empty_response_retry_delay_ms: 2000,
            empty_response_max_delay_ms: 20_000,
            disable_synthetic_thought_signature: false,
            tool_id_recovery: true,
            claude_tool_hardening: true,
            oauth_server_bind_all: false,
            health_score: HealthScoreConfig::default(),
            token_bucket: TokenBucketConfig::default(),
        }
    }
}

impl AntigravityConfig {
    /// Clamp numeric settings into safe bounded ranges.
    pub fn clamp(&mut self) {
        self.soft_quota_threshold_percent = self.soft_quota_threshold_percent.min(100);
        self.max_cache_first_wait_seconds = self.max_cache_first_wait_seconds.clamp(1, 3600);
        self.default_retry_after_seconds = self.default_retry_after_seconds.clamp(1, 3600);
        self.max_backoff_seconds = self.max_backoff_seconds.clamp(5, 3600);
        self.max_rate_limit_wait_seconds = self.max_rate_limit_wait_seconds.min(3600);
        self.request_jitter_max_ms = self.request_jitter_max_ms.min(5000);
        self.soft_quota_cache_ttl_minutes = self.soft_quota_cache_ttl_minutes.clamp(1, 120);
        self.quota_refresh_interval_minutes = self.quota_refresh_interval_minutes.min(1440);
        self.proactive_refresh_buffer_seconds =
            self.proactive_refresh_buffer_seconds.clamp(60, 3500);
        self.proactive_refresh_interval_seconds =
            self.proactive_refresh_interval_seconds.clamp(30, 3600);
        self.empty_response_max_attempts = self.empty_response_max_attempts.clamp(1, 10);
        self.empty_response_retry_delay_ms = self.empty_response_retry_delay_ms.clamp(100, 60_000);
        self.empty_response_max_delay_ms = self
            .empty_response_max_delay_ms
            .clamp(self.empty_response_retry_delay_ms, 300_000);
        self.health_score.max_score = self.health_score.max_score.clamp(10, 1000);
        self.health_score.min_usable = self
            .health_score
            .min_usable
            .clamp(0, self.health_score.max_score);
        self.health_score.initial = self
            .health_score
            .initial
            .clamp(self.health_score.min_usable, self.health_score.max_score);
        self.token_bucket.max_tokens = self.token_bucket.max_tokens.clamp(1, 1000);
        self.token_bucket.initial_tokens = self
            .token_bucket
            .initial_tokens
            .clamp(0, self.token_bucket.max_tokens);
        if self.token_bucket.regeneration_rate_per_minute <= 0.0 {
            self.token_bucket.regeneration_rate_per_minute = 1.0;
        }
    }

    /// Delay before empty-response retry `attempt` (0-based), with exponential
    /// growth capped at [`Self::empty_response_max_delay_ms`]. Saturating `u64`
    /// math keeps large attempt counts from overflowing before the cap.
    #[must_use]
    pub fn empty_response_backoff_ms(&self, attempt: u32) -> u64 {
        let factor = 1u64 << attempt.min(31);
        self.empty_response_retry_delay_ms
            .saturating_mul(factor)
            .min(self.empty_response_max_delay_ms)
    }

    /// Merge JSONC contents on top of the current configuration.
    pub fn merge_jsonc(&mut self, jsonc_source: &str) -> Result<()> {
        let parsed = jsonc_parser::parse_to_serde_value(
            jsonc_source,
            &jsonc_parser::ParseOptions::default(),
        )
        .map_err(|err| AntigravityError::Config(format!("failed to parse JSONC: {err}")))?;
        let Some(override_val) = parsed else {
            return Ok(());
        };
        let mut base = serde_json::to_value(&*self)?;
        deep_merge(&mut base, &override_val);
        *self = serde_json::from_value(base).map_err(|err| {
            AntigravityError::Config(format!("invalid antigravity config: {err}"))
        })?;
        self.clamp();
        Ok(())
    }

    /// Apply `OPENCODE_ANTIGRAVITY_*` environment variable overrides.
    pub fn apply_env_overrides(&mut self) {
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_KEEP_THINKING") {
            self.keep_thinking = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_SESSION_RECOVERY") {
            self.session_recovery = v;
        }
        if let Ok(v) = std::env::var("OPENCODE_ANTIGRAVITY_ACCOUNT_SELECTION_STRATEGY") {
            match v.trim().to_ascii_lowercase().as_str() {
                "sticky" => self.account_selection_strategy = AccountSelectionStrategy::Sticky,
                "round-robin" | "round_robin" => {
                    self.account_selection_strategy = AccountSelectionStrategy::RoundRobin;
                }
                "hybrid" => self.account_selection_strategy = AccountSelectionStrategy::Hybrid,
                _ => {}
            }
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_PID_OFFSET_ENABLED") {
            self.pid_offset_enabled = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_SWITCH_ON_FIRST_RATE_LIMIT") {
            self.switch_on_first_rate_limit = v;
        }
        if let Ok(v) = std::env::var("OPENCODE_ANTIGRAVITY_SCHEDULING_MODE") {
            match v.trim().to_ascii_lowercase().as_str() {
                "cache-first" | "cache_first" => {
                    self.scheduling_mode = SchedulingMode::CacheFirst;
                }
                "balance" => self.scheduling_mode = SchedulingMode::Balance,
                "performance-first" | "performance_first" => {
                    self.scheduling_mode = SchedulingMode::PerformanceFirst;
                }
                _ => {}
            }
        }
        if let Some(v) = env_u8("OPENCODE_ANTIGRAVITY_SOFT_QUOTA_THRESHOLD_PERCENT") {
            self.soft_quota_threshold_percent = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_QUOTA_FALLBACK") {
            self.quota_fallback = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_CLI_FIRST") {
            self.cli_first = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_OAUTH_SERVER_BIND_ALL") {
            self.oauth_server_bind_all = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_CLAUDE_TOOL_HARDENING") {
            self.claude_tool_hardening = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_TOOL_ID_RECOVERY") {
            self.tool_id_recovery = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_AUTO_UPDATE") {
            self.auto_update = v;
        }
        if let Some(v) = env_u64("OPENCODE_ANTIGRAVITY_EMPTY_RESPONSE_RETRY_DELAY_MS") {
            self.empty_response_retry_delay_ms = v;
        }
        if let Some(v) = env_u64("OPENCODE_ANTIGRAVITY_EMPTY_RESPONSE_MAX_DELAY_MS") {
            self.empty_response_max_delay_ms = v;
        }
        if let Some(v) = env_bool("OPENCODE_ANTIGRAVITY_DISABLE_SYNTHETIC_THOUGHT_SIGNATURE") {
            self.disable_synthetic_thought_signature = v;
        }
        self.clamp();
    }
}

/// Resolve the base `opencode` configuration directory used for Antigravity state files.
#[must_use]
pub fn resolve_config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("OPENCODE_ANTIGRAVITY_CONFIG_DIR")
        && !override_dir.trim().is_empty()
    {
        return PathBuf::from(override_dir);
    }
    if cfg!(windows)
        && let Ok(appdata) = std::env::var("APPDATA")
        && !appdata.trim().is_empty()
    {
        return PathBuf::from(appdata).join("opencode");
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg).join("opencode");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home).join(".config").join("opencode");
    }
    PathBuf::from(".opencode")
}

/// Discover and load `antigravity.jsonc` / `antigravity.json` (user config -> project config -> env).
pub fn load_config(
    project_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Result<AntigravityConfig> {
    let mut cfg = AntigravityConfig::default();

    let user_dir = config_dir.map_or_else(resolve_config_dir, PathBuf::from);
    for candidate in [
        user_dir.join("antigravity.json"),
        user_dir.join("antigravity.jsonc"),
    ] {
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate)?;
            cfg.merge_jsonc(&text)?;
        }
    }

    if let Some(proj) = project_dir {
        for candidate in [
            proj.join(".opencode").join("antigravity.json"),
            proj.join(".opencode").join("antigravity.jsonc"),
        ] {
            if candidate.is_file() {
                let text = std::fs::read_to_string(&candidate)?;
                cfg.merge_jsonc(&text)?;
            }
        }
    }

    cfg.apply_env_overrides();
    Ok(cfg)
}

fn deep_merge(target: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (target, overlay) {
        (serde_json::Value::Object(t_map), serde_json::Value::Object(o_map)) => {
            for (k, v) in o_map {
                match t_map.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        t_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (t_val, o_val) => *t_val = o_val.clone(),
    }
}

fn env_bool(key: &str) -> Option<bool> {
    let val = std::env::var(key).ok()?;
    match val.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

fn env_u8(key: &str) -> Option<u8> {
    std::env::var(key).ok()?.trim().parse::<u8>().ok()
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn jsonc_overlay_and_clamping_work() {
        let dir = TempDir::new().expect("tempdir");
        let user_dir = dir.path().join("user");
        let proj_dir = dir.path().join("proj");
        std::fs::create_dir_all(&user_dir).expect("mkdir");
        std::fs::create_dir_all(proj_dir.join(".opencode")).expect("mkdir");

        std::fs::write(
            user_dir.join("antigravity.jsonc"),
            r#"{
                // user defaults
                "account_selection_strategy": "round-robin",
                "soft_quota_threshold_percent": 150,
                "signature_cache": { "memory_ttl_seconds": 1200 }
            }"#,
        )
        .expect("write user");

        std::fs::write(
            proj_dir.join(".opencode/antigravity.json"),
            r#"{"cli_first": true, "claude_tool_hardening": false}"#,
        )
        .expect("write proj");

        let cfg = load_config(Some(&proj_dir), Some(&user_dir)).expect("load config");
        assert_eq!(
            cfg.account_selection_strategy,
            AccountSelectionStrategy::RoundRobin
        );
        assert_eq!(cfg.soft_quota_threshold_percent, 100);
        assert_eq!(cfg.signature_cache.memory_ttl_seconds, 1200);
        assert_eq!(cfg.signature_cache.disk_ttl_seconds, 172_800);
        assert!(cfg.cli_first);
        assert!(!cfg.claude_tool_hardening);
    }

    #[test]
    fn empty_response_backoff_is_exponential_and_capped() {
        let cfg = AntigravityConfig {
            empty_response_retry_delay_ms: 1000,
            empty_response_max_delay_ms: 5000,
            ..AntigravityConfig::default()
        };
        let sequence: Vec<u64> = (0..8).map(|n| cfg.empty_response_backoff_ms(n)).collect();
        assert_eq!(
            sequence,
            vec![1000, 2000, 4000, 5000, 5000, 5000, 5000, 5000]
        );
        // A malformed attempt count must not overflow into a tiny/zero delay.
        assert_eq!(cfg.empty_response_backoff_ms(u32::MAX), 5000);
    }

    #[test]
    fn default_empty_response_settings_are_sane() {
        let cfg = AntigravityConfig::default();
        assert_eq!(cfg.empty_response_max_attempts, 4);
        assert_eq!(cfg.empty_response_retry_delay_ms, 2000);
        assert_eq!(cfg.empty_response_max_delay_ms, 20_000);
        assert_eq!(cfg.empty_response_backoff_ms(0), 2000);
        assert_eq!(cfg.empty_response_backoff_ms(1), 4000);
        assert_eq!(cfg.empty_response_backoff_ms(2), 8000);
        assert_eq!(cfg.empty_response_backoff_ms(3), 16_000);
        assert_eq!(cfg.empty_response_backoff_ms(4), 20_000);
        assert!(!cfg.disable_synthetic_thought_signature);
    }

    #[test]
    fn empty_response_config_can_be_overridden_and_clamped() {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("antigravity.jsonc"),
            r#"{
                "empty_response_max_attempts": 99,
                "empty_response_retry_delay_ms": 10000000,
                "empty_response_max_delay_ms": 1000,
                "disable_synthetic_thought_signature": true
            }"#,
        )
        .expect("write config");
        let cfg = load_config(None, Some(dir.path())).expect("load config");
        assert_eq!(cfg.empty_response_max_attempts, 10);
        assert_eq!(cfg.empty_response_retry_delay_ms, 60_000);
        // The cap cannot be lower than the base after clamping.
        assert_eq!(cfg.empty_response_max_delay_ms, 60_000);
        assert!(cfg.disable_synthetic_thought_signature);
    }
}
