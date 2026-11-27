//! Pure rate budgets. Callers pass explicit millisecond timestamps; there is
//! no clock in this crate.
//!
//! [`WriteBudget`] limits `session_write` calls per sender. The budget is
//! per-sender and in-memory: a runtime restart forgets every window, while the
//! inbox rows a write already produced stay durable in the store. The
//! `steer`/`queue` delivery choice does not bypass the budget — it is consumed
//! before the row is written, so a refused write never leaves a partial row and
//! changing delivery does not buy extra writes.

use crate::{BudgetError, Limits};

/// Default spawns per minute when the runtime does not override it.
pub const DEFAULT_SPAWNS_PER_MIN: u32 = 8;

/// Rate window length.
pub const WINDOW_MS: i64 = 60_000;

#[derive(Debug, Clone)]
struct Window {
    start: Option<i64>,
    count: u32,
}

impl Window {
    const fn new() -> Self {
        Self {
            start: None,
            count: 0,
        }
    }

    /// Start a new window when absent, expired, or the clock went backwards.
    fn advance(&mut self, now_ms: i64) {
        match self.start {
            Some(start) if now_ms >= start && now_ms - start < WINDOW_MS => {}
            _ => {
                self.start = Some(now_ms);
                self.count = 0;
            }
        }
    }

    fn retry_after_ms(&self, now_ms: i64) -> i64 {
        self.start
            .map_or(0, |start| (start + WINDOW_MS - now_ms).max(0))
    }

    fn try_take(&mut self, now_ms: i64, max_per_minute: u32) -> Result<(), BudgetError> {
        self.try_take_batch(now_ms, max_per_minute, 1)
    }

    fn try_take_batch(
        &mut self,
        now_ms: i64,
        max_per_minute: u32,
        count: u32,
    ) -> Result<(), BudgetError> {
        self.advance(now_ms);
        if self.count.saturating_add(count) > max_per_minute {
            return Err(BudgetError::RateLimited {
                max_per_minute,
                retry_after_ms: self.retry_after_ms(now_ms),
            });
        }
        self.count += count;
        Ok(())
    }
}

/// Spawn budget for one parent session: a lifetime cap and a per-minute cap.
#[derive(Debug, Clone)]
pub struct SpawnBudget {
    per_session: u32,
    per_minute: u32,
    total: u32,
    window: Window,
}

impl SpawnBudget {
    #[must_use]
    pub fn new(per_session: u32, per_minute: u32) -> Self {
        Self {
            per_session,
            per_minute,
            total: 0,
            window: Window::new(),
        }
    }

    /// A budget with the default 8 spawns/minute and `per_session` total.
    #[must_use]
    pub fn with_default_rate(per_session: u32) -> Self {
        Self::new(per_session, DEFAULT_SPAWNS_PER_MIN)
    }

    /// Build from the parent's `max_children` lifetime allowance.
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self::with_default_rate(limits.max_children)
    }

    /// Sync active children count so completed children release their concurrency slot.
    pub fn sync_active(&mut self, active_count: u32) {
        self.total = active_count.min(self.per_session);
    }

    /// Update per-session and per-minute spawn limits when session policy changes (e.g. `/complex`).
    pub fn reconfigure(&mut self, per_session: u32, per_minute: u32) {
        self.per_session = per_session;
        self.per_minute = per_minute;
        self.total = self.total.min(self.per_session);
    }

    pub fn try_spawn(&mut self, now_ms: i64) -> Result<(), BudgetError> {
        self.try_spawn_batch(1, now_ms)
    }

    /// Atomically reserve `count` spawn slots against both session and rate limits.
    pub fn try_spawn_batch(&mut self, count: u32, now_ms: i64) -> Result<(), BudgetError> {
        if self.total.saturating_add(count) > self.per_session {
            return Err(BudgetError::SessionLimit {
                max: self.per_session,
            });
        }
        self.window.try_take_batch(now_ms, self.per_minute, count)?;
        self.total += count;
        Ok(())
    }

    #[must_use]
    pub const fn total(&self) -> u32 {
        self.total
    }
}

/// Message budget for one sender.
#[derive(Debug, Clone)]
pub struct SiblingBudget {
    max_per_minute: u32,
    window: Window,
}

impl SiblingBudget {
    #[must_use]
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            window: Window::new(),
        }
    }

    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self::new(limits.max_messages_per_min)
    }

    pub fn try_message(&mut self, now_ms: i64) -> Result<(), BudgetError> {
        self.window.try_take(now_ms, self.max_per_minute)
    }
}

/// `session_write` budget for one sender.
#[derive(Debug, Clone)]
pub struct WriteBudget {
    max_per_minute: u32,
    window: Window,
}

impl WriteBudget {
    #[must_use]
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            window: Window::new(),
        }
    }

    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self::new(limits.max_writes_per_min)
    }

    pub fn try_write(&mut self, now_ms: i64) -> Result<(), BudgetError> {
        self.window.try_take(now_ms, self.max_per_minute)
    }
}

/// Micro-cents per USD (`1 uc = $0.00001`).
pub const MICRO_CENTS_PER_USD: u64 = 100_000;

/// Convert a non-negative finite USD figure into micro-cents (`1 uc = $0.00001`).
#[must_use]
pub fn usd_to_uc(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    let scaled = usd * 100_000.0;
    if scaled >= 18_446_744_073_709_551_615.0 {
        return u64::MAX;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled.round() as u64
    }
}

/// Convert micro-cents back into USD.
#[must_use]
pub fn uc_to_usd(uc: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        (uc as f64) / 100_000.0
    }
}

/// Affine pre-reservation token issued before a single LLM call.
///
/// Intentionally implements neither `Clone` nor `Copy` so it must be settled
/// once against its owning [`CostBudget`].
#[derive(Debug, PartialEq, Eq)]
pub struct BudgetReservation {
    reserved_uc: u64,
}

impl BudgetReservation {
    #[must_use]
    pub fn reserved_uc(&self) -> u64 {
        self.reserved_uc
    }
}

/// Affine USD cost budget for autonomous turns and subagent trees.
///
/// Intentionally implements neither `Clone` nor `Copy`: Rust's ownership rules
/// forbid aliasing or double-spending a parent budget across concurrent subagents
/// at compile time (`docs/30-tos-safe-harness.md`, arXiv:2606.04056).
#[derive(Debug, PartialEq, Eq)]
pub struct CostBudget {
    remaining_uc: u64,
}

impl CostBudget {
    /// Create a budget denominated in micro-cents (`1 uc = $0.00001`).
    #[must_use]
    pub fn new(total_uc: u64) -> Self {
        Self {
            remaining_uc: total_uc,
        }
    }

    /// Create a budget from a USD cap.
    #[must_use]
    pub fn from_usd(usd: f64) -> Self {
        Self::new(usd_to_uc(usd))
    }

    /// Remaining uncommitted micro-cents.
    #[must_use]
    pub fn remaining_uc(&self) -> u64 {
        self.remaining_uc
    }

    /// Reserve `estimated_uc` atomically before issuing a model call.
    pub fn reserve(&mut self, estimated_uc: u64) -> Result<BudgetReservation, BudgetError> {
        let Some(next) = self.remaining_uc.checked_sub(estimated_uc) else {
            return Err(BudgetError::CostExceeded {
                needed_uc: estimated_uc,
                remaining_uc: self.remaining_uc,
            });
        };
        self.remaining_uc = next;
        Ok(BudgetReservation {
            reserved_uc: estimated_uc,
        })
    }

    /// Reconcile a finished model call's actual cost against its reservation.
    ///
    /// Unused pre-reserved balance is refunded; an overage charges remaining
    /// headroom and returns [`BudgetError::CostExceeded`] if it exceeded the cap.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "affine reservation token is consumed by value to prevent double settlement"
    )]
    pub fn settle(
        &mut self,
        reservation: BudgetReservation,
        actual_uc: u64,
    ) -> Result<(), BudgetError> {
        let BudgetReservation { reserved_uc } = reservation;
        if actual_uc <= reserved_uc {
            self.remaining_uc = self.remaining_uc.saturating_add(reserved_uc - actual_uc);
            return Ok(());
        }
        let extra = actual_uc - reserved_uc;
        if let Some(next) = self.remaining_uc.checked_sub(extra) {
            self.remaining_uc = next;
            Ok(())
        } else {
            let remaining = self.remaining_uc;
            self.remaining_uc = 0;
            Err(BudgetError::CostExceeded {
                needed_uc: extra,
                remaining_uc: remaining,
            })
        }
    }

    /// Split off `child_uc` into an independent owned child budget for a spawned
    /// subagent (`checked_sub`).
    pub fn split(&mut self, child_uc: u64) -> Result<CostBudget, BudgetError> {
        let Some(next) = self.remaining_uc.checked_sub(child_uc) else {
            return Err(BudgetError::CostExceeded {
                needed_uc: child_uc,
                remaining_uc: self.remaining_uc,
            });
        };
        self.remaining_uc = next;
        Ok(CostBudget::new(child_uc))
    }

    /// Reclaim unused funds from a settled child subagent budget.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "affine child budget is moved back into parent by value"
    )]
    pub fn reclaim(&mut self, child: CostBudget) {
        let CostBudget { remaining_uc } = child;
        self.remaining_uc = self.remaining_uc.saturating_add(remaining_uc);
    }
}

#[cfg(test)]
mod tests {
    use super::{BudgetError, CostBudget, Limits, WINDOW_MS, WriteBudget, uc_to_usd};

    #[test]
    fn write_budget_allows_twenty_per_minute() {
        let mut budget = WriteBudget::from_limits(&Limits::default());
        for tick in 0..20 {
            assert!(budget.try_write(tick * 100).is_ok());
        }
        assert_eq!(
            budget.try_write(2_000),
            Err(BudgetError::RateLimited {
                max_per_minute: 20,
                retry_after_ms: 58_000,
            })
        );
    }

    #[test]
    fn write_budget_admits_again_after_the_window_expires() {
        let mut budget = WriteBudget::new(2);
        assert!(budget.try_write(0).is_ok());
        assert!(budget.try_write(1_000).is_ok());
        assert!(matches!(
            budget.try_write(2_000),
            Err(BudgetError::RateLimited { .. })
        ));
        assert!(budget.try_write(WINDOW_MS - 1).is_err());
        assert!(budget.try_write(WINDOW_MS).is_ok());
    }

    #[test]
    fn write_budget_is_per_sender() {
        let limits = Limits::default();
        let mut alice = WriteBudget::from_limits(&limits);
        let mut bob = WriteBudget::from_limits(&limits);
        for tick in 0..20 {
            assert!(alice.try_write(tick).is_ok());
        }
        assert!(alice.try_write(100).is_err());
        assert!(
            bob.try_write(100).is_ok(),
            "a second sender has its own window"
        );
    }

    #[test]
    fn write_budget_honors_a_configured_limit() {
        let limits = Limits {
            max_writes_per_min: 2,
            ..Limits::default()
        };
        let mut budget = WriteBudget::from_limits(&limits);
        assert!(budget.try_write(0).is_ok());
        assert!(budget.try_write(0).is_ok());
        assert_eq!(
            budget.try_write(0),
            Err(BudgetError::RateLimited {
                max_per_minute: 2,
                retry_after_ms: 60_000,
            })
        );
    }

    #[test]
    fn write_budget_zero_denies_every_write() {
        // A hard zero is total denial, not "unlimited": `max_writes_per_min: 0`
        // is the honest reading of a zero write allowance, and it stays denied
        // even after a window would have expired.
        let limits = Limits {
            max_writes_per_min: 0,
            ..Limits::default()
        };
        let mut budget = WriteBudget::from_limits(&limits);
        assert_eq!(
            budget.try_write(0),
            Err(BudgetError::RateLimited {
                max_per_minute: 0,
                retry_after_ms: 60_000,
            })
        );
        assert_eq!(
            budget.try_write(WINDOW_MS),
            Err(BudgetError::RateLimited {
                max_per_minute: 0,
                retry_after_ms: 60_000,
            })
        );
    }

    #[test]
    fn spawn_budget_batch_is_atomic_on_session_and_rate_limits() {
        let mut budget = super::SpawnBudget::new(3, 5);
        assert!(budget.try_spawn_batch(4, 100).is_err());
        assert_eq!(budget.total(), 0);
        assert!(budget.try_spawn_batch(3, 100).is_ok());
        assert_eq!(budget.total(), 3);
    }

    #[test]
    fn affine_cost_budget_reserves_splits_settles_and_reclaims_cleanly() {
        let mut parent = CostBudget::from_usd(1.00);
        assert_eq!(parent.remaining_uc(), 100_000);

        let res = parent.reserve(30_000).expect("reserve $0.30");
        assert_eq!(parent.remaining_uc(), 70_000);
        assert_eq!(res.reserved_uc(), 30_000);

        // Over-splitting past remaining uncommitted funds must fail cleanly.
        assert_eq!(
            parent.split(80_000),
            Err(BudgetError::CostExceeded {
                needed_uc: 80_000,
                remaining_uc: 70_000,
            })
        );

        let mut child = parent.split(40_000).expect("split $0.40");
        assert_eq!(parent.remaining_uc(), 30_000);
        assert_eq!(child.remaining_uc(), 40_000);

        // Child spends 10k uc out of 25k reservation, then returns remainder to parent.
        let child_res = child.reserve(25_000).expect("child reserve");
        child.settle(child_res, 10_000).expect("child settle");
        assert_eq!(child.remaining_uc(), 30_000);
        parent.reclaim(child);
        assert_eq!(parent.remaining_uc(), 60_000);

        // Parent settles with partial refund (spent 20k out of 30k).
        parent.settle(res, 20_000).expect("parent settle");
        assert_eq!(parent.remaining_uc(), 70_000);
        assert!((uc_to_usd(parent.remaining_uc()) - 0.70).abs() < 1e-9);
    }
}
