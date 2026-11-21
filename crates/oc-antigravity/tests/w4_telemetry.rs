//! Wave 4C (I5) offline telemetry regressions: logging must not change retry accounting,
//! token-bucket net balance, or the stale-quota fail-open decision.
//!
//! No HTTP, no credentials, no service touch. Uses only public pure helpers and in-memory
//! account storage, so it is safe to run while the live service stays up.

use std::collections::HashMap;

use oc_antigravity::ModelFamily;
use oc_antigravity::accounts::AccountManager;
use oc_antigravity::config::{AccountSelectionStrategy, AntigravityConfig, TokenBucketConfig};
use oc_antigravity::constants::HeaderStyle;
use oc_antigravity::rotation::{
    RateLimitReason, TokenBucketTracker, calculate_backoff_ms, parse_rate_limit_reason,
};
use oc_antigravity::storage::{
    AccountMetadataV4, AccountStorageV4, CachedAccountQuota, QuotaGroupSnapshot,
};

fn account(refresh: &str) -> AccountMetadataV4 {
    AccountMetadataV4 {
        email: Some(format!("w4-{refresh}@example.test")),
        label: None,
        refresh_token: refresh.to_owned(),
        project_id: Some("proj-test".to_owned()),
        managed_project_id: None,
        added_at: 0,
        last_used: 0,
        enabled: true,
        rate_limit_reset_times: HashMap::default(),
        cached_tier: None,
        cached_quota: None,
        verification_required: false,
        verification_required_at: None,
        verification_required_reason: None,
        verification_url: None,
        revoked: false,
        fingerprint: None,
        fingerprint_history: Vec::new(),
        token_hash: String::new(),
        store_credential_id: None,
    }
}

#[test]
fn rate_limit_classification_and_backoff_are_unchanged_by_logging() {
    assert_eq!(
        parse_rate_limit_reason(429, "RATE_LIMIT_EXCEEDED"),
        RateLimitReason::RateLimitExceeded
    );
    assert_eq!(
        parse_rate_limit_reason(429, "Resource has been exhausted (check quota)"),
        RateLimitReason::QuotaExhausted
    );
    assert_eq!(
        parse_rate_limit_reason(503, "upstream overload"),
        RateLimitReason::ServerError
    );
    assert_eq!(
        parse_rate_limit_reason(400, "other"),
        RateLimitReason::Unknown
    );
    // The observed classification still drives the same cooldown (default 60s, cap 60s).
    assert_eq!(
        calculate_backoff_ms(RateLimitReason::QuotaExhausted, 1, None, 60_000, 60_000),
        60_000
    );
    assert_eq!(
        calculate_backoff_ms(
            RateLimitReason::RateLimitExceeded,
            1,
            Some(90_000),
            60_000,
            60_000
        ),
        90_000
    );
}

#[test]
fn refund_after_successful_consume_is_net_zero() {
    let cfg = TokenBucketConfig {
        max_tokens: 10,
        initial_tokens: 5,
        regeneration_rate_per_minute: 0.0,
    };
    let mut tracker = TokenBucketTracker::new(cfg);
    let now = 1_700_000_000_000;
    let key = "w4-test-key";
    assert_eq!(tracker.available(key, now), 5.0);
    assert!(tracker.try_consume(key, now));
    assert_eq!(tracker.available(key, now), 4.0);
    tracker.refund(key, now);
    assert_eq!(
        tracker.available(key, now),
        5.0,
        "consume+refund must net zero"
    );
}

#[test]
fn stale_quota_snapshot_fails_open_and_selection_is_unchanged() {
    let cfg = AntigravityConfig {
        account_selection_strategy: AccountSelectionStrategy::Sticky,
        soft_quota_threshold_percent: 90,
        soft_quota_cache_ttl_minutes: 10,
        token_bucket: TokenBucketConfig {
            max_tokens: 10,
            initial_tokens: 10,
            regeneration_rate_per_minute: 0.0,
        },
        ..AntigravityConfig::default()
    };
    let now = 1_700_000_000_000;
    let mut quota = CachedAccountQuota {
        updated_at: now - 11 * 60_000,
        ..CachedAccountQuota::default()
    };
    quota.groups.insert(
        "gemini-antigravity".to_owned(),
        QuotaGroupSnapshot {
            used_percent: Some(99.0),
            remaining_fraction: Some(0.0),
            reset_time_ms: None,
            ..QuotaGroupSnapshot::default()
        },
    );
    let mut storage = AccountStorageV4::default();
    let mut acct = account("w4-refresh-token|proj");
    acct.cached_quota = Some(quota);
    storage.accounts.push(acct);
    let mut mgr = AccountManager::from_storage(None, storage, cfg);

    // A stale 99%-used snapshot must fail open: not blocked, no cooldown, selectable.
    assert!(!mgr.is_soft_quota_exceeded(
        0,
        ModelFamily::Gemini,
        HeaderStyle::Antigravity,
        "gemini-3.1-pro",
        now
    ));
    let selected = mgr
        .select_for_request(
            ModelFamily::Gemini,
            HeaderStyle::Antigravity,
            "gemini-3.1-pro",
            now,
            &[],
        )
        .expect("stale quota must still allow selection");
    assert_eq!(selected.index, 0);
    assert!(mgr.accounts()[0].rate_limit_reset_times.is_empty());
}
