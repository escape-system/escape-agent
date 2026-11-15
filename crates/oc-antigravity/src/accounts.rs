//! Multi-account pool manager supporting sticky, round-robin, and hybrid rotation,
//! per-family rate-limit tracking, soft quota gates, verification flags, and disk sync.

use crate::config::{AccountSelectionStrategy, AntigravityConfig, SchedulingMode};
use crate::constants::HeaderStyle;
use crate::error::{AntigravityError, Result};
use crate::fingerprint::{FingerprintVersion, MAX_FINGERPRINT_HISTORY, generate_fingerprint};
use crate::rotation::{
    HealthScoreTracker, HybridCandidate, RateLimitReason, TokenBucketTracker, calculate_backoff_ms,
    select_hybrid_account,
};
use crate::storage::{
    AccountMetadataV4, AccountStorageV4, CachedAccountQuota, account_token_hash, load_accounts,
    save_accounts,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Coarse model family used for independent sticky cursors and base quota buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFamily {
    /// Claude models served through Antigravity Cloud Code.
    Claude,
    /// Gemini models served through Antigravity Hub or Gemini CLI quota pools.
    Gemini,
}

impl ModelFamily {
    /// Canonical storage cursor key (`"claude"` or `"gemini"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }

    /// Infer [`ModelFamily`] from a resolved or raw model name.
    #[must_use]
    pub fn from_model(model: &str) -> Self {
        if model.to_ascii_lowercase().contains("claude") {
            Self::Claude
        } else {
            Self::Gemini
        }
    }
}

/// Build the primary quota key used to isolate rate limits by family, header style, and model.
#[must_use]
pub fn quota_key(family: ModelFamily, style: HeaderStyle, model: Option<&str>) -> String {
    let base = match family {
        ModelFamily::Claude => "claude".to_owned(),
        ModelFamily::Gemini => match style {
            HeaderStyle::Antigravity => "gemini-antigravity".to_owned(),
            HeaderStyle::GeminiCli => "gemini-cli".to_owned(),
        },
    };
    if let Some(m) = model {
        // Variant-insensitive: `model#high` and `model#xhigh` are the same upstream quota
        // bucket, so a cooldown recorded for one must apply to the other.
        let trimmed = m.trim().split('#').next().unwrap_or_default().trim();
        if !trimmed.is_empty() {
            return format!("{base}:{}", trimmed.to_ascii_lowercase());
        }
    }
    base
}

/// Determine the quota-cache group name used for soft-quota threshold checks.
#[must_use]
pub fn soft_quota_group(family: ModelFamily, style: HeaderStyle, model: &str) -> &'static str {
    if family == ModelFamily::Claude {
        return "claude";
    }
    if style == HeaderStyle::GeminiCli {
        return "gemini-cli";
    }
    let lower = model.to_ascii_lowercase();
    if lower.contains("flash") || lower.contains("hydrogen") {
        "gemini-flash"
    } else {
        "gemini-pro"
    }
}

/// Selection outcome produced by [`AccountManager::select_for_request`].
#[derive(Debug, Clone)]
pub struct SelectedAccount {
    /// Index in the account pool.
    pub index: usize,
    /// Snapshot of account metadata at selection time.
    pub account: AccountMetadataV4,
}

/// Account manager owning the persistent pool and live rotation state.
#[derive(Debug)]
pub struct AccountManager {
    storage_path: Option<PathBuf>,
    storage: AccountStorageV4,
    health: HealthScoreTracker,
    buckets: TokenBucketTracker,
    cfg: AntigravityConfig,
    cursor_offset: usize,
}

impl AccountManager {
    /// Load or initialize an [`AccountManager`] backed by `storage_path` (if provided).
    pub fn load(storage_path: Option<PathBuf>, cfg: AntigravityConfig) -> Result<Self> {
        let storage = match &storage_path {
            Some(path) => load_accounts(path)?,
            None => AccountStorageV4::default(),
        };
        Ok(Self::from_storage(storage_path, storage, cfg))
    }

    /// Construct from an in-memory [`AccountStorageV4`].
    #[must_use]
    pub fn from_storage(
        storage_path: Option<PathBuf>,
        mut storage: AccountStorageV4,
        cfg: AntigravityConfig,
    ) -> Self {
        let cursor_offset = if cfg.pid_offset_enabled && !storage.accounts.is_empty() {
            (std::process::id() as usize) % storage.accounts.len()
        } else {
            0
        };
        for acct in &mut storage.accounts {
            acct.ensure_token_hash();
        }
        let health = HealthScoreTracker::new(cfg.health_score.clone());
        let buckets = TokenBucketTracker::new(cfg.token_bucket.clone());
        Self {
            storage_path,
            storage,
            health,
            buckets,
            cfg,
            cursor_offset,
        }
    }

    /// Replace configuration at runtime without discarding persisted accounts.
    pub fn update_config(&mut self, cfg: AntigravityConfig) {
        self.cfg = cfg;
    }

    /// Storage file path, if file-backed.
    #[must_use]
    pub fn storage_path(&self) -> Option<&Path> {
        self.storage_path.as_deref()
    }

    /// Number of configured accounts in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.storage.accounts.len()
    }

    /// Return `true` when zero accounts are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.storage.accounts.is_empty()
    }

    /// Immutable view of all stored accounts.
    #[must_use]
    pub fn accounts(&self) -> &[AccountMetadataV4] {
        &self.storage.accounts
    }

    /// Immutable snapshot of V4 storage.
    #[must_use]
    pub fn storage_snapshot(&self) -> &AccountStorageV4 {
        &self.storage
    }

    /// Persist changes to disk if configured with a `storage_path`.
    pub fn save(&self) -> Result<()> {
        if let Some(ref path) = self.storage_path {
            save_accounts(path, &self.storage)?;
        }
        Ok(())
    }

    /// Add or update an account by email (or refresh token) and persist.
    pub fn upsert_account(
        &mut self,
        mut account: AccountMetadataV4,
        make_active: bool,
    ) -> Result<usize> {
        account.ensure_token_hash();
        let existing_idx = self.storage.accounts.iter().position(|existing| {
            if let (Some(a), Some(b)) = (&existing.email, &account.email)
                && a.eq_ignore_ascii_case(b)
            {
                return true;
            }
            existing.refresh_token == account.refresh_token
                || existing.token_hash == account.token_hash
        });

        let idx = if let Some(pos) = existing_idx {
            let prev = &self.storage.accounts[pos];
            if account.added_at == 0 {
                account.added_at = prev.added_at;
            }
            if account.fingerprint.is_none() {
                account.fingerprint.clone_from(&prev.fingerprint);
            }
            if account.fingerprint_history.is_empty() {
                account
                    .fingerprint_history
                    .clone_from(&prev.fingerprint_history);
            }
            if account.store_credential_id.is_none() {
                account
                    .store_credential_id
                    .clone_from(&prev.store_credential_id);
            }
            self.storage.accounts[pos] = account;
            pos
        } else {
            self.storage.accounts.push(account);
            self.storage.accounts.len() - 1
        };

        if make_active {
            self.storage.active_index = idx;
            self.storage
                .active_index_by_family
                .insert("claude".to_owned(), idx);
            self.storage
                .active_index_by_family
                .insert("gemini".to_owned(), idx);
        }

        self.save()?;
        Ok(idx)
    }

    /// Find an account index by `"all"`/`1`-based or `0`-based index, email, label, `token_hash`, or `store_credential_id`.
    #[must_use]
    pub fn find_account(&self, identifier: &str) -> Option<usize> {
        let needle = identifier.trim();
        if needle.is_empty() {
            return None;
        }
        if let Ok(idx) = needle.parse::<usize>() {
            if idx < self.storage.accounts.len() {
                return Some(idx);
            }
            if idx >= 1 && idx <= self.storage.accounts.len() {
                return Some(idx - 1);
            }
        }
        self.storage.accounts.iter().position(|a| {
            a.token_hash.eq_ignore_ascii_case(needle)
                || a.email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(needle))
                || a.label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(needle))
                || a.store_credential_id
                    .as_deref()
                    .is_some_and(|id| id == needle)
        })
    }

    /// Set the sticky active account across both families.
    pub fn select_account(&mut self, index: usize) -> Result<()> {
        if index >= self.storage.accounts.len() {
            return Err(AntigravityError::NoAccounts(format!(
                "account index {index} out of bounds"
            )));
        }
        self.storage.active_index = index;
        self.storage
            .active_index_by_family
            .insert("claude".to_owned(), index);
        self.storage
            .active_index_by_family
            .insert("gemini".to_owned(), index);
        self.save()
    }

    /// Enable or disable an account.
    pub fn set_enabled(&mut self, index: usize, enabled: bool) -> Result<()> {
        let acct = self.storage.accounts.get_mut(index).ok_or_else(|| {
            AntigravityError::NoAccounts(format!("account index {index} out of bounds"))
        })?;
        acct.enabled = enabled;
        self.save()
    }

    /// Remove an account from the pool, adjusting cursors safely, and returning the removed entry.
    pub fn remove_account(&mut self, index: usize) -> Result<AccountMetadataV4> {
        if index >= self.storage.accounts.len() {
            return Err(AntigravityError::NoAccounts(format!(
                "account index {index} out of bounds"
            )));
        }
        let removed = self.storage.accounts.remove(index);
        if self.storage.accounts.is_empty() {
            self.storage.active_index = 0;
            self.storage.active_index_by_family.clear();
        } else {
            let max_idx = self.storage.accounts.len() - 1;
            self.storage.active_index = self.storage.active_index.min(max_idx);
            for cursor in self.storage.active_index_by_family.values_mut() {
                *cursor = (*cursor).min(max_idx);
            }
        }
        self.save()?;
        Ok(removed)
    }

    /// Check whether an account at `index` is currently rate-limited for `family` / `style` / `model`.
    #[must_use]
    pub fn is_rate_limited(
        &self,
        index: usize,
        family: ModelFamily,
        style: HeaderStyle,
        model: Option<&str>,
        now_ms: i64,
    ) -> bool {
        let Some(acct) = self.storage.accounts.get(index) else {
            return true;
        };
        let specific = quota_key(family, style, model);
        let base = quota_key(family, style, None);
        [specific, base].iter().any(|k| {
            acct.rate_limit_reset_times
                .get(k)
                .is_some_and(|&reset_at| reset_at > now_ms)
        })
    }

    /// Check whether an account exceeds configured soft quota threshold using its cached quota.
    #[must_use]
    pub fn is_soft_quota_exceeded(
        &self,
        index: usize,
        family: ModelFamily,
        style: HeaderStyle,
        model: &str,
        now_ms: i64,
    ) -> bool {
        if self.cfg.soft_quota_threshold_percent >= 100 {
            return false;
        }
        let Some(acct) = self.storage.accounts.get(index) else {
            return false;
        };
        let Some(ref cached) = acct.cached_quota else {
            return false;
        };
        let ttl_ms = i64::try_from(self.cfg.soft_quota_cache_ttl_minutes.saturating_mul(60_000))
            .unwrap_or(600_000);
        if now_ms - cached.updated_at > ttl_ms {
            return false;
        }
        let group = soft_quota_group(family, style, model);
        let Some(snapshot) = cached.groups.get(group) else {
            return false;
        };
        let used = snapshot.used_percent.or_else(|| {
            snapshot
                .remaining_fraction
                .map(|rem| ((1.0 - rem.clamp(0.0, 1.0)) * 100.0).clamp(0.0, 100.0))
        });
        used.is_some_and(|u| u >= f64::from(self.cfg.soft_quota_threshold_percent))
    }

    /// Return the active sticky cursor for `family`.
    #[must_use]
    pub fn active_cursor(&self, family: ModelFamily) -> usize {
        if self.storage.accounts.is_empty() {
            return 0;
        }
        let base = self
            .storage
            .active_index_by_family
            .get(family.as_str())
            .copied()
            .unwrap_or(self.storage.active_index);
        (base + self.cursor_offset) % self.storage.accounts.len()
    }

    /// Select an eligible account for the given `family` / `style` / `model`.
    /// Returns `None` if all enabled non-revoked accounts are currently rate-limited or blocked.
    pub fn select_for_request(
        &mut self,
        family: ModelFamily,
        style: HeaderStyle,
        model: &str,
        now_ms: i64,
        excluded_indices: &[usize],
    ) -> Option<SelectedAccount> {
        self.clear_expired_rate_limits(now_ms);
        if self.storage.accounts.is_empty() {
            return None;
        }

        let sticky = self.active_cursor(family);
        let eligible: Vec<usize> = (0..self.storage.accounts.len())
            .filter(|&i| {
                let acct = &self.storage.accounts[i];
                acct.enabled
                    && !acct.revoked
                    && !acct.verification_required
                    && !excluded_indices.contains(&i)
                    && !self.is_rate_limited(i, family, style, Some(model), now_ms)
                    && !self.is_soft_quota_exceeded(i, family, style, model, now_ms)
            })
            .collect();

        if eligible.is_empty() {
            return None;
        }

        let picked = match self.cfg.account_selection_strategy {
            AccountSelectionStrategy::Sticky => {
                if eligible.contains(&sticky) {
                    sticky
                } else {
                    eligible[0]
                }
            }
            AccountSelectionStrategy::RoundRobin => {
                let start = (sticky + 1) % self.storage.accounts.len();
                let mut found = eligible[0];
                for offset in 0..self.storage.accounts.len() {
                    let cand = (start + offset) % self.storage.accounts.len();
                    if eligible.contains(&cand) {
                        found = cand;
                        break;
                    }
                }
                found
            }
            AccountSelectionStrategy::Hybrid => {
                // Keep sticky account in `cache-first` unless unhealthy or starved.
                if self.cfg.scheduling_mode == SchedulingMode::CacheFirst
                    && eligible.contains(&sticky)
                    && self
                        .health
                        .is_usable(&self.storage.accounts[sticky].token_hash, now_ms)
                    && self
                        .buckets
                        .available(&self.storage.accounts[sticky].token_hash, now_ms)
                        >= 1.0
                {
                    sticky
                } else {
                    let candidates: Vec<HybridCandidate<'_>> = eligible
                        .iter()
                        .map(|&idx| {
                            let acct = &self.storage.accounts[idx];
                            HybridCandidate {
                                index: idx,
                                key: acct.token_hash.as_str(),
                                last_used_ms: acct.last_used,
                                is_sticky: idx == sticky,
                            }
                        })
                        .collect();
                    select_hybrid_account(
                        &candidates,
                        &mut self.health,
                        &mut self.buckets,
                        self.cfg.health_score.max_score,
                        self.cfg.token_bucket.max_tokens,
                        now_ms,
                    )
                    .unwrap_or(eligible[0])
                }
            }
        };

        self.storage
            .active_index_by_family
            .insert(family.as_str().to_owned(), picked);
        self.storage.active_index = picked;
        if let Some(acct) = self.storage.accounts.get_mut(picked) {
            acct.last_used = now_ms;
            let _ = self.buckets.try_consume(&acct.token_hash, now_ms);
            return Some(SelectedAccount {
                index: picked,
                account: acct.clone(),
            });
        }
        None
    }

    /// Shortest wait (ms) until ANY enabled non-revoked non-verification-blocked account becomes free.
    #[must_use]
    pub fn min_wait_time_ms(
        &self,
        family: ModelFamily,
        style: HeaderStyle,
        model: Option<&str>,
        now_ms: i64,
    ) -> Option<i64> {
        let specific = quota_key(family, style, model);
        let base = quota_key(family, style, None);
        let mut min_wait: Option<i64> = None;

        for acct in &self.storage.accounts {
            if !acct.enabled || acct.revoked || acct.verification_required {
                continue;
            }
            let wait_for_acct = [specific.as_str(), base.as_str()]
                .iter()
                .filter_map(|k| acct.rate_limit_reset_times.get(*k))
                .map(|&reset| (reset - now_ms).max(0))
                .max()
                .unwrap_or(0);
            min_wait = Some(min_wait.map_or(wait_for_acct, |prev| prev.min(wait_for_acct)));
        }

        min_wait
    }

    /// Count enabled, non-revoked, non-verification-blocked accounts.
    #[must_use]
    pub fn usable_account_count(&self) -> usize {
        self.storage
            .accounts
            .iter()
            .filter(|a| a.enabled && !a.revoked && !a.verification_required)
            .count()
    }

    /// Record a successful upstream call on `index`.
    pub fn mark_success(&mut self, index: usize, now_ms: i64) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            acct.last_used = now_ms;
            self.health.record_success(&acct.token_hash, now_ms);
        }
        self.save()
    }

    /// Refund 1 token bucket credit if a local/transport error aborted before calling upstream.
    pub fn refund_token(&mut self, index: usize, now_ms: i64) {
        if let Some(acct) = self.storage.accounts.get(index) {
            self.buckets.refund(&acct.token_hash, now_ms);
        }
    }

    /// Record a rate limit / transient capacity hit and compute cooldown reset timestamp.
    #[allow(clippy::too_many_arguments)]
    pub fn mark_rate_limited(
        &mut self,
        index: usize,
        family: ModelFamily,
        style: HeaderStyle,
        model: Option<&str>,
        reason: RateLimitReason,
        retry_after_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<i64> {
        let default_ms = i64::try_from(self.cfg.default_retry_after_seconds.saturating_mul(1000))
            .unwrap_or(60_000);
        let max_ms =
            i64::try_from(self.cfg.max_backoff_seconds.saturating_mul(1000)).unwrap_or(60_000);

        let key = quota_key(family, style, model);
        let Some(acct) = self.storage.accounts.get_mut(index) else {
            return Ok(default_ms);
        };
        let failures = self.health.record_rate_limit(&acct.token_hash, now_ms);
        let delay = calculate_backoff_ms(reason, failures, retry_after_ms, default_ms, max_ms);
        let reset_at = now_ms + delay;
        acct.rate_limit_reset_times.insert(key, reset_at);
        if model.is_some() {
            let base = quota_key(family, style, None);
            // Only apply short capacity pause on base key when capacity exhausted across models.
            if matches!(reason, RateLimitReason::QuotaExhausted) {
                acct.rate_limit_reset_times.insert(base, reset_at);
            }
        }
        self.save()?;
        Ok(delay)
    }

    /// Record a non-429 failure on `index` (returns current consecutive failures).
    pub fn mark_failure(&mut self, index: usize, now_ms: i64) -> u32 {
        if let Some(acct) = self.storage.accounts.get(index) {
            self.health.record_failure(&acct.token_hash, now_ms)
        } else {
            1
        }
    }

    /// Flag an account as requiring Google identity verification (`403 validation_required`).
    pub fn mark_verification_required(
        &mut self,
        index: usize,
        reason: Option<String>,
        url: Option<String>,
        now_ms: i64,
    ) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            acct.verification_required = true;
            acct.verification_required_at = Some(now_ms);
            acct.verification_required_reason = reason;
            acct.verification_url = url;
        }
        self.save()
    }

    /// Clear `verification_required` flags (e.g. after re-enabling or re-login).
    pub fn clear_verification_required(&mut self, index: usize) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            acct.verification_required = false;
            acct.verification_required_at = None;
            acct.verification_required_reason = None;
            acct.verification_url = None;
        }
        self.save()
    }

    /// Mark an account's refresh token permanently revoked (`invalid_grant`).
    pub fn mark_revoked(&mut self, index: usize) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            acct.revoked = true;
        }
        self.save()
    }

    /// Update an account's refresh token after Google token rotation.
    pub fn apply_token_rotation(
        &mut self,
        index: usize,
        new_refresh_token: String,
    ) -> Result<Option<String>> {
        let Some(acct) = self.storage.accounts.get_mut(index) else {
            return Ok(None);
        };
        if acct.refresh_token == new_refresh_token {
            return Ok(None);
        }
        let store_id = acct.store_credential_id.clone();
        acct.refresh_token = new_refresh_token;
        acct.token_hash = account_token_hash(&acct.refresh_token);
        acct.revoked = false;
        self.save()?;
        Ok(store_id)
    }

    /// Update an account's `store_credential_id`.
    pub fn set_store_credential_id(&mut self, index: usize, cred_id: String) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            acct.store_credential_id = Some(cred_id);
        }
        self.save()
    }

    /// Cache the managed project ID and subscription tier discovered via `loadCodeAssist`.
    ///
    /// Only fields that changed are rewritten, and nothing is persisted when both values
    /// already match the stored account.
    pub fn cache_project_context(
        &mut self,
        index: usize,
        managed_project_id: Option<String>,
        tier_id: Option<String>,
    ) -> Result<()> {
        let Some(acct) = self.storage.accounts.get_mut(index) else {
            return Ok(());
        };
        let mut changed = false;
        if let Some(managed) = managed_project_id
            && acct.managed_project_id.as_deref() != Some(managed.as_str())
        {
            acct.managed_project_id = Some(managed);
            changed = true;
        }
        if let Some(tier) = tier_id
            && acct.cached_tier.as_deref() != Some(tier.as_str())
        {
            acct.cached_tier = Some(tier);
            changed = true;
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// Update cached quota snapshot and optional tier label on an account.
    pub fn update_quota_snapshot(
        &mut self,
        index: usize,
        quota: CachedAccountQuota,
        tier: Option<String>,
    ) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            acct.cached_quota = Some(quota);
            if tier.is_some() {
                acct.cached_tier = tier;
            }
        }
        self.save()
    }

    /// Archive existing fingerprint and generate a new one for `index`.
    pub fn regenerate_fingerprint(
        &mut self,
        index: usize,
        reason: &str,
        now_ms: i64,
        hub_version: &str,
    ) -> Result<()> {
        if let Some(acct) = self.storage.accounts.get_mut(index) {
            if let Some(prev) = acct.fingerprint.take() {
                acct.fingerprint_history.insert(
                    0,
                    FingerprintVersion {
                        fingerprint: prev,
                        timestamp: now_ms,
                        reason: reason.to_owned(),
                    },
                );
                acct.fingerprint_history.truncate(MAX_FINGERPRINT_HISTORY);
            }
            acct.fingerprint = Some(generate_fingerprint(now_ms, hub_version));
        }
        self.save()
    }

    /// Remove rate limit entries whose `reset_at <= now_ms`.
    pub fn clear_expired_rate_limits(&mut self, now_ms: i64) {
        for acct in &mut self.storage.accounts {
            acct.rate_limit_reset_times
                .retain(|_, reset_at| *reset_at > now_ms);
        }
    }

    /// Current health score snapshot by index (useful for CLI/HTTP status endpoints).
    pub fn health_score_for(&mut self, index: usize, now_ms: i64) -> Option<f64> {
        let acct = self.storage.accounts.get(index)?;
        Some(self.health.score(&acct.token_hash, now_ms))
    }

    /// Return rate-limit keys still active at `now_ms` for account `index`.
    #[must_use]
    pub fn active_rate_limits(&self, index: usize, now_ms: i64) -> HashMap<String, i64> {
        let Some(acct) = self.storage.accounts.get(index) else {
            return HashMap::new();
        };
        acct.rate_limit_reset_times
            .iter()
            .filter(|&(_, &reset)| reset > now_ms)
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_account(email: &str, token: &str) -> AccountMetadataV4 {
        AccountMetadataV4 {
            email: Some(email.to_owned()),
            label: None,
            refresh_token: token.to_owned(),
            project_id: Some("p-1".to_owned()),
            managed_project_id: None,
            added_at: 1000,
            last_used: 0,
            enabled: true,
            rate_limit_reset_times: HashMap::new(),
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
    fn sticky_cursor_stays_on_active_account_until_rate_limited() {
        let cfg = AntigravityConfig {
            account_selection_strategy: AccountSelectionStrategy::Sticky,
            ..Default::default()
        };
        let mut mgr = AccountManager::from_storage(None, AccountStorageV4::default(), cfg);

        mgr.upsert_account(sample_account("a@ex.com", "t-a"), true)
            .expect("add a");
        mgr.upsert_account(sample_account("b@ex.com", "t-b"), false)
            .expect("add b");

        let now = 10_000;
        let first = mgr
            .select_for_request(
                ModelFamily::Gemini,
                HeaderStyle::Antigravity,
                "gemini-3.1-pro",
                now,
                &[],
            )
            .expect("select 1");
        assert_eq!(first.index, 0);

        mgr.mark_rate_limited(
            0,
            ModelFamily::Gemini,
            HeaderStyle::Antigravity,
            Some("gemini-3.1-pro"),
            RateLimitReason::QuotaExhausted,
            Some(30_000),
            now,
        )
        .expect("mark limit");

        let second = mgr
            .select_for_request(
                ModelFamily::Gemini,
                HeaderStyle::Antigravity,
                "gemini-3.1-pro",
                now + 1000,
                &[],
            )
            .expect("select 2");
        assert_eq!(second.index, 1);

        // Claude family remains independent and can still use account 0!
        let claude = mgr
            .select_for_request(
                ModelFamily::Claude,
                HeaderStyle::Antigravity,
                "claude-sonnet-4-6",
                now + 1000,
                &[],
            )
            .expect("select claude");
        assert_eq!(claude.index, 0);
    }
}
