//! Pure rate budgets. Callers pass explicit millisecond timestamps; there is
//! no clock in this crate.

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
        self.advance(now_ms);
        if self.count >= max_per_minute {
            return Err(BudgetError::RateLimited {
                max_per_minute,
                retry_after_ms: self.retry_after_ms(now_ms),
            });
        }
        self.count += 1;
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

    pub fn try_spawn(&mut self, now_ms: i64) -> Result<(), BudgetError> {
        if self.total >= self.per_session {
            return Err(BudgetError::SessionLimit {
                max: self.per_session,
            });
        }
        self.window.try_take(now_ms, self.per_minute)?;
        self.total += 1;
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
