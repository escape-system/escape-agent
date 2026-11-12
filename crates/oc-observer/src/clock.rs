//! Injectable time sources.
//!
//! Two clocks are exposed on purpose:
//!
//! - [`Clock::now_ms`] is the wall clock (epoch ms). It is published and
//!   audited, and it can jump forwards or backwards when NTP corrects it.
//! - [`Clock::mono_ms`] is a process-monotonic millisecond counter. It never
//!   jumps, so every deadline, elapsed time and retention TTL uses it.
//!
//! Production uses [`SystemClock`]; tests use a fake clock so no test sleeps.

use std::sync::OnceLock;
use std::time::Instant;

/// A wall clock plus a process-monotonic counter.
pub trait Clock: Send + Sync {
    /// Current wall-clock time in epoch milliseconds.
    fn now_ms(&self) -> i64;

    /// Milliseconds since the process monotonic anchor.
    ///
    /// The default body is the process-wide [`Instant`] anchor
    /// ([`process_mono_ms`]), so an implementation that only cares about wall
    /// time stays source-compatible. **A test fake must override this**,
    /// otherwise it silently follows real time and its deadline assertions
    /// become flaky.
    fn mono_ms(&self) -> i64 {
        process_mono_ms()
    }
}

/// Milliseconds since the process monotonic anchor.
///
/// The anchor is a `OnceLock<Instant>` captured on first use; [`Instant`] never
/// goes backwards, so the value is monotonic for the life of the process.
#[must_use]
pub fn process_mono_ms() -> i64 {
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    let anchor = ANCHOR.get_or_init(Instant::now);
    i64::try_from(anchor.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Both clocks read at the same instant.
///
/// Threaded through the engine so a single decision cannot accidentally mix a
/// wall instant with a monotonic one: wall values are only for published or
/// audited timestamps, monotonic values are only for deadlines, elapsed time
/// and TTLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineNow {
    /// Wall clock, epoch ms. Published and audited only.
    pub wall_ms: i64,
    /// Process-monotonic ms. Deadlines, elapsed time and TTLs only.
    pub mono_ms: i64,
}

impl EngineNow {
    /// Read both clocks once.
    #[must_use]
    pub fn from_clock(clock: &dyn Clock) -> Self {
        Self {
            wall_ms: clock.now_ms(),
            mono_ms: clock.mono_ms(),
        }
    }
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

    /// Production monotonic reading: the process-wide [`Instant`] anchor.
    ///
    /// Explicit rather than inherited so the production choice is stated where
    /// `SystemClock` is defined, and so a future default change cannot silently
    /// move production onto a different time source.
    fn mono_ms(&self) -> i64 {
        process_mono_ms()
    }
}
