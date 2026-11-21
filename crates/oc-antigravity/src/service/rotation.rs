//! Request-time account rotation bookkeeping: selection, waits, success/rate-limit/failure marks.

use super::AntigravityService;
use super::sync::lock_accounts;
use crate::accounts::{ModelFamily, SelectedAccount};
use crate::constants::HeaderStyle;
use crate::error::Result;
use crate::rotation::RateLimitReason;
use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

impl AntigravityService {
    pub(crate) fn select_account_for_request(
        &self,
        family: ModelFamily,
        style: HeaderStyle,
        model: &str,
        excluded: &[usize],
    ) -> Result<Option<SelectedAccount>> {
        let mut mgr = lock_accounts(&self.inner.accounts)?;
        Ok(mgr.select_for_request(family, style, model, now_ms(), excluded))
    }

    pub(crate) fn min_wait_ms(
        &self,
        family: ModelFamily,
        style: HeaderStyle,
        model: Option<&str>,
    ) -> Result<Option<i64>> {
        let mgr = lock_accounts(&self.inner.accounts)?;
        Ok(mgr.min_wait_time_ms(family, style, model, now_ms()))
    }

    pub(crate) fn mark_success(&self, index: usize) {
        if let Ok(mut mgr) = lock_accounts(&self.inner.accounts) {
            let _ = mgr.mark_success(index, now_ms());
        }
    }

    pub(crate) fn refund_token(&self, index: usize) {
        if let Ok(mut mgr) = lock_accounts(&self.inner.accounts) {
            mgr.refund_token(index, now_ms());
        }
    }

    pub(crate) fn mark_rate_limited(
        &self,
        index: usize,
        family: ModelFamily,
        style: HeaderStyle,
        model: Option<&str>,
        reason: RateLimitReason,
        retry_after_ms: Option<i64>,
    ) -> i64 {
        if let Ok(mut mgr) = lock_accounts(&self.inner.accounts) {
            mgr.mark_rate_limited(
                index,
                family,
                style,
                model,
                reason,
                retry_after_ms,
                now_ms(),
            )
            .unwrap_or(60_000)
        } else {
            60_000
        }
    }

    pub(crate) fn mark_verification_required(
        &self,
        index: usize,
        reason: Option<String>,
        url: Option<String>,
    ) {
        if let Ok(mut mgr) = lock_accounts(&self.inner.accounts) {
            let _ = mgr.mark_verification_required(index, reason, url, now_ms());
        }
    }

    pub(crate) fn mark_failure(&self, index: usize) -> u32 {
        lock_accounts(&self.inner.accounts).map_or(1, |mut mgr| mgr.mark_failure(index, now_ms()))
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub(super) fn short_random_hex() -> String {
    let mut b = [0u8; 12];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}
