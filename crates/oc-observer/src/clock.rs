//! Injectable time source.
//!
//! Production uses [`SystemClock`]; tests use a fake clock so no test sleeps.

/// A monotonic-enough wall clock in epoch milliseconds.
pub trait Clock: Send + Sync {
    /// Current time in epoch milliseconds.
    fn now_ms(&self) -> i64;
}

/// Wall clock backed by the operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}
