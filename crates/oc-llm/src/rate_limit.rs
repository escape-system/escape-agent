//! Proactive GCRA rate limiting (`governor`) and sliding spend-velocity guard.
//!
//! Protects API-based providers (especially Google AI Studio Tier 1's 10-minute
//! rolling spend window and per-key RPM/TPM bounds) from burst storms before an
//! HTTP request is even placed on the wire (`docs/30-tos-safe-harness.md`).

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroU32;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use governor::clock::{Clock, DefaultClock};
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use tokio::sync::Semaphore;

/// Micro-cents per USD (`1 uc = $0.00001`).
pub const MICRO_CENTS_PER_USD: f64 = 100_000.0;

/// Rolling spend window duration (10 minutes for Google AI Studio Tier 1).
const SPEND_WINDOW: Duration = Duration::from_secs(600);

/// Per-provider proactive rate limits and optional spend-velocity ceiling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitPolicy {
    /// Requests per minute ceiling (0 disables RPM gating).
    pub rpm: u32,
    /// Estimated tokens per minute ceiling (0 disables TPM gating).
    pub tpm: u32,
    /// Maximum USD spend inside any rolling 10-minute window (`None` disables).
    pub spend_cap_10m_usd: Option<f64>,
    /// Maximum concurrent in-flight requests per provider key (`None` disables).
    pub max_concurrent: Option<usize>,
}

impl RateLimitPolicy {
    /// Construct a policy for explicit RPM/TPM/spend thresholds.
    #[must_use]
    pub fn new(rpm: u32, tpm: u32, spend_cap_10m_usd: Option<f64>) -> Self {
        Self {
            rpm,
            tpm,
            spend_cap_10m_usd,
            max_concurrent: None,
        }
    }
}

type KeyedLimiter = RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

/// Result of attempting to pass the proactive GCRA / spend guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    Allowed,
    Delay {
        wait: Duration,
        reason: &'static str,
    },
}

/// Owned concurrency guard; callers drop this before sleeping on 429 retries so
/// parked tasks never starve live traffic.
#[derive(Debug)]
pub struct RateLimitPermit {
    _semaphore: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// Process-wide proactive rate limiter keyed by `(provider, credential_slot)`.
#[derive(Debug)]
pub struct ProviderRateLimiter {
    clock: DefaultClock,
    policies: RwLock<BTreeMap<String, RateLimitPolicy>>,
    rpm_limiters: RwLock<BTreeMap<String, Arc<KeyedLimiter>>>,
    tpm_limiters: RwLock<BTreeMap<String, Arc<KeyedLimiter>>>,
    spend_windows: RwLock<BTreeMap<String, VecDeque<(Instant, u64)>>>,
    semaphores: RwLock<BTreeMap<String, Arc<Semaphore>>>,
}

impl Default for ProviderRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRateLimiter {
    /// Empty limiter (no limits active until configured via [`Self::set_policy`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            clock: DefaultClock::default(),
            policies: RwLock::new(BTreeMap::new()),
            rpm_limiters: RwLock::new(BTreeMap::new()),
            tpm_limiters: RwLock::new(BTreeMap::new()),
            spend_windows: RwLock::new(BTreeMap::new()),
            semaphores: RwLock::new(BTreeMap::new()),
        }
    }

    /// Install or replace the proactive policy for `provider` (case-insensitive).
    pub fn set_policy(&self, provider: &str, policy: RateLimitPolicy) {
        let id = provider.trim().to_ascii_lowercase();
        if let Ok(mut guard) = self.policies.write() {
            guard.insert(id.clone(), policy);
        }
        if let Some(rpm) = NonZeroU32::new(policy.rpm)
            && let Ok(mut guard) = self.rpm_limiters.write()
        {
            guard.insert(
                id.clone(),
                Arc::new(RateLimiter::dashmap(Quota::per_minute(rpm))),
            );
        } else if let Ok(mut guard) = self.rpm_limiters.write() {
            guard.remove(&id);
        }
        if let Some(tpm) = NonZeroU32::new(policy.tpm)
            && let Ok(mut guard) = self.tpm_limiters.write()
        {
            // Keep burst capacity capped to the full minute quota so an entire
            // prompt's token estimate can pass if within the minute budget.
            guard.insert(
                id.clone(),
                Arc::new(RateLimiter::dashmap(Quota::per_minute(tpm))),
            );
        } else if let Ok(mut guard) = self.tpm_limiters.write() {
            guard.remove(&id);
        }
        if let Some(conc) = policy.max_concurrent.filter(|&c| c > 0)
            && let Ok(mut guard) = self.semaphores.write()
        {
            guard.insert(id, Arc::new(Semaphore::new(conc)));
        } else if let Ok(mut guard) = self.semaphores.write() {
            guard.remove(&id);
        }
    }

    /// Clear every configured policy (useful in tests).
    pub fn clear(&self) {
        if let Ok(mut g) = self.policies.write() {
            g.clear();
        }
        if let Ok(mut g) = self.rpm_limiters.write() {
            g.clear();
        }
        if let Ok(mut g) = self.tpm_limiters.write() {
            g.clear();
        }
        if let Ok(mut g) = self.spend_windows.write() {
            g.clear();
        }
        if let Ok(mut g) = self.semaphores.write() {
            g.clear();
        }
    }

    /// Check RPM, TPM, and rolling 10-minute USD spend ceiling once without sleeping.
    pub fn check_once(
        &self,
        provider: &str,
        bucket_key: &str,
        estimated_tokens: u32,
        estimated_cost_usd: f64,
    ) -> RateLimitDecision {
        let id = provider.trim().to_ascii_lowercase();
        let policy = self.policies.read().ok().and_then(|g| g.get(&id).copied());
        let Some(policy) = policy else {
            return RateLimitDecision::Allowed;
        };

        if let Some(rpm) = self
            .rpm_limiters
            .read()
            .ok()
            .and_then(|g| g.get(&id).cloned())
            && let Err(not_until) = rpm.check_key(&bucket_key.to_owned())
        {
            return RateLimitDecision::Delay {
                wait: not_until.wait_time_from(self.clock.now()),
                reason: "proactive GCRA RPM limit",
            };
        }

        if let Some(tpm) = self
            .tpm_limiters
            .read()
            .ok()
            .and_then(|g| g.get(&id).cloned())
        {
            let clamped = estimated_tokens.clamp(1, policy.tpm.max(1));
            if let Some(cells) = NonZeroU32::new(clamped) {
                let wait = match tpm.check_key_n(&bucket_key.to_owned(), cells) {
                    Ok(Ok(())) => None,
                    Ok(Err(not_until)) => Some(not_until.wait_time_from(self.clock.now())),
                    Err(_) => Some(Duration::from_millis(250)),
                };
                if let Some(wait) = wait {
                    return RateLimitDecision::Delay {
                        wait,
                        reason: "proactive GCRA TPM limit",
                    };
                }
            }
        }

        if let Some(cap_usd) = policy
            .spend_cap_10m_usd
            .filter(|&v| v.is_finite() && v > 0.0)
        {
            let cap_uc = usd_to_uc(cap_usd);
            let cost_uc = usd_to_uc(estimated_cost_usd.max(0.0));
            let now = Instant::now();
            if let Ok(mut windows) = self.spend_windows.write() {
                let window = windows.entry(bucket_key.to_owned()).or_default();
                prune_window(window, now);
                let used: u64 = window.iter().map(|(_, uc)| *uc).sum();
                if used.saturating_add(cost_uc) > cap_uc
                    && let Some((oldest, _)) = window.front()
                {
                    let elapsed = now.saturating_duration_since(*oldest);
                    let wait = SPEND_WINDOW
                        .saturating_sub(elapsed)
                        .max(Duration::from_millis(50));
                    return RateLimitDecision::Delay {
                        wait,
                        reason: "Tier 1 10-minute spend velocity cap",
                    };
                }
                if cost_uc > 0 {
                    window.push_back((now, cost_uc));
                }
            }
        }

        RateLimitDecision::Allowed
    }

    /// Acquire a concurrency permit (after waiting for any GCRA / spend cooldown
    /// without holding any semaphore permits during sleep).
    pub async fn acquire(
        &self,
        provider: &str,
        credential_slot: &str,
        estimated_tokens: u32,
        estimated_cost_usd: f64,
    ) -> RateLimitPermit {
        let id = provider.trim().to_ascii_lowercase();
        let bucket_key = format!("{id}:{credential_slot}");
        loop {
            match self.check_once(&id, &bucket_key, estimated_tokens, estimated_cost_usd) {
                RateLimitDecision::Allowed => break,
                RateLimitDecision::Delay { wait, reason } => {
                    tracing::debug!(
                        provider = %id,
                        wait_ms = wait.as_millis(),
                        reason,
                        "throttling outbound request via proactive GCRA limiter"
                    );
                    tokio::time::sleep(wait.min(Duration::from_secs(30))).await;
                }
            }
        }
        let sem = self
            .semaphores
            .read()
            .ok()
            .and_then(|g| g.get(&id).cloned());
        let permit = match sem {
            Some(sem) => sem.acquire_owned().await.ok(),
            None => None,
        };
        RateLimitPermit { _semaphore: permit }
    }

    /// Record actual post-step USD spend into the sliding 10-minute window.
    pub fn record_spend(&self, provider: &str, credential_slot: &str, actual_usd: f64) {
        if !actual_usd.is_finite() || actual_usd <= 0.0 {
            return;
        }
        let bucket_key = format!("{}:{credential_slot}", provider.trim().to_ascii_lowercase());
        let uc = usd_to_uc(actual_usd);
        if uc == 0 {
            return;
        }
        let now = Instant::now();
        if let Ok(mut windows) = self.spend_windows.write() {
            let window = windows.entry(bucket_key).or_default();
            prune_window(window, now);
            window.push_back((now, uc));
        }
    }
}

fn prune_window(window: &mut VecDeque<(Instant, u64)>, now: Instant) {
    while let Some((ts, _)) = window.front() {
        if now.saturating_duration_since(*ts) >= SPEND_WINDOW {
            window.pop_front();
        } else {
            break;
        }
    }
}

fn usd_to_uc(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    let scaled = usd * MICRO_CENTS_PER_USD;
    if scaled >= 18_446_744_073_709_551_615.0 {
        return u64::MAX;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled.round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcra_rpm_allows_burst_and_delays_excess() {
        let limiter = ProviderRateLimiter::new();
        limiter.set_policy("google", RateLimitPolicy::new(2, 0, None));
        assert_eq!(
            limiter.check_once("google", "google:default", 10, 0.0),
            RateLimitDecision::Allowed
        );
        assert_eq!(
            limiter.check_once("google", "google:default", 10, 0.0),
            RateLimitDecision::Allowed
        );
        assert!(matches!(
            limiter.check_once("google", "google:default", 10, 0.0),
            RateLimitDecision::Delay {
                reason: "proactive GCRA RPM limit",
                ..
            }
        ));
    }

    #[test]
    fn sliding_spend_velocity_cap_blocks_excess_in_window() {
        let limiter = ProviderRateLimiter::new();
        limiter.set_policy("google", RateLimitPolicy::new(1_000, 0, Some(10.0)));
        limiter.record_spend("google", "key1", 9.0);
        assert!(matches!(
            limiter.check_once("google", "google:key1", 10, 2.0),
            RateLimitDecision::Delay {
                reason: "Tier 1 10-minute spend velocity cap",
                ..
            }
        ));
        assert_eq!(
            limiter.check_once("google", "google:key1", 10, 0.5),
            RateLimitDecision::Allowed
        );
    }
}
