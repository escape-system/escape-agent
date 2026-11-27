//! Provider retry policy.
//!
//! Exponential 2 s → 10 s per gap, 10 attempts, optional jitter, and a
//! provider-requested `retry-after` capped at 60 s. Timeout retries are
//! counted separately because a timed-out attempt has already waited.
//!
//! A server hint is a floor, never a ceiling: the delay is
//! `max(backoff, retry_after)`, so jitter can spread our own backoff but can
//! never shorten the wait the provider asked for.

use std::time::Duration;

use crate::error::LlmError;

/// Retry configuration. Durations are short in tests.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub max: Duration,
    pub jitter: bool,
    pub max_retry_after: Duration,
    pub max_timeout_retries: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            base: Duration::from_secs(2),
            max: Duration::from_secs(10),
            jitter: true,
            // One minute is long enough to honor every hint a live provider
            // sends (the observed 429s ask for 1-28 s) while keeping a single
            // `stream()` call from blocking for a quarter hour. `Timeouts::total`
            // is unset by default, so this is the only bound on the wait.
            max_retry_after: Duration::from_secs(60),
            max_timeout_retries: 3,
        }
    }
}

impl RetryPolicy {
    /// A single-attempt policy (`max_attempts: 1`, `max_timeout_retries: 0`)
    /// used when an outer runtime owns retries and key rotation so retry counts
    /// never multiply across layers (`docs/30-tos-safe-harness.md`).
    #[must_use]
    pub fn single_attempt() -> Self {
        Self {
            max_attempts: 1,
            max_timeout_retries: 0,
            ..Self::default()
        }
    }

    /// A deterministic policy used by tests.
    #[must_use]
    pub fn fixed(base: Duration, max: Duration, max_attempts: u32) -> Self {
        Self {
            max_attempts,
            base,
            max,
            jitter: false,
            ..Self::default()
        }
    }

    /// Delay before retry number `attempt` (1 = the first retry).
    ///
    /// `retry_after` is the provider's own instruction. It is a floor: the
    /// returned delay is never shorter than the hint (or than
    /// `max_retry_after` when the hint exceeds it), and jitter is applied only
    /// to our own exponential backoff, so spreading retries can never make us
    /// hammer a provider that asked us to wait.
    #[must_use]
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exponent = attempt.saturating_sub(1).min(20);
        let factor = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let base = self.base.saturating_mul(factor).min(self.max);
        let backoff = if self.jitter {
            // ±25% keeps the exponential shape while spreading retries.
            let roll: f64 = rand::random();
            let scale = 0.75 + roll * 0.5;
            base.mul_f64(scale)
        } else {
            base
        };
        let Some(hint) = retry_after else {
            return backoff;
        };
        // Clamp the hint before comparing so an absurd value can neither park
        // the turn nor overflow the arithmetic below, and add positive jitter on
        // top of server hints when jitter is enabled.
        let hint = hint.min(self.max_retry_after);
        let hinted = if self.jitter && hint > Duration::ZERO {
            let max_extra = (hint / 10).min(Duration::from_millis(500));
            let roll: f64 = rand::random();
            hint.saturating_add(max_extra.mul_f64(roll))
        } else {
            hint
        };
        backoff.max(hinted).min(self.max.max(self.max_retry_after))
    }
}

/// Mutable attempt state for one call.
#[derive(Debug, Clone)]
pub struct RetryState {
    pub policy: RetryPolicy,
    pub attempt: u32,
    pub timeouts: u32,
}

impl RetryState {
    #[must_use]
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            timeouts: 0,
        }
    }

    /// Record a failed attempt and return the delay, or `None` when exhausted.
    pub fn next_delay(&mut self, error: &LlmError) -> Option<Duration> {
        if !error.is_retryable() && !matches!(error, LlmError::Timeout { .. }) {
            return None;
        }
        if self.attempt + 1 >= self.policy.max_attempts {
            return None;
        }
        if matches!(error, LlmError::Timeout { .. }) {
            self.timeouts += 1;
            if self.timeouts > self.policy.max_timeout_retries {
                return None;
            }
        }
        self.attempt += 1;
        let retry_after = error.retry_after_ms().map(Duration::from_millis);
        Some(self.policy.delay(self.attempt, retry_after))
    }
}

/// Parse a `retry-after` header value.
///
/// Supports both forms RFC 9110 defines: `delta-seconds` (`28`) and an
/// HTTP-date (`Sun, 06 Nov 1994 08:49:37 GMT`). Fractional seconds are
/// tolerated because test servers use them to keep suites fast; a real
/// provider never sends them.
///
/// Unparsable, negative, non-finite, and out-of-range values return `None` so
/// the caller falls back to exponential backoff. This never panics: an
/// overflowing delta (`1e300`) is rejected by [`Duration::try_from_secs_f64`]
/// rather than handed to the panicking `Duration::from_secs_f64`.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        // `from_secs_f64` panics when the value exceeds `Duration`'s range;
        // the fallible constructor reports the same input as `None`.
        return Duration::try_from_secs_f64(seconds).ok();
    }
    parse_http_date(value).map(|deadline| {
        // A date far in the future is a valid IMF-fixdate but an absurd wait;
        // the policy caps it, and the wrap to zero cannot happen because the
        // subtraction saturates at 0 for past dates.
        Duration::from_secs(u64::try_from(deadline).unwrap_or(u64::MAX))
    })
}

/// Seconds between `value` and the epoch, or `0` for a date in the past.
///
/// A past date means "retry immediately", so it yields zero rather than a
/// negative duration.
fn parse_http_date(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = parts.next()?.parse::<i64>().ok()?;
    let mut time = parts.next()?.split(':');
    let hour = time.next()?.parse::<i64>().ok()?;
    let minute = time.next()?.parse::<i64>().ok()?;
    let second = time.next()?.parse::<i64>().ok()?;
    if !parts.next()?.eq_ignore_ascii_case("GMT") {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second.min(59);
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
    .unwrap_or(i64::MAX);
    // A past date means "retry immediately". `saturating_sub` would clamp to
    // `i64::MIN`, which then wraps to `u64::MAX` in the caller, so compare
    // explicitly instead of relying on saturation.
    Some(if seconds > now { seconds - now } else { 0 })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_is_capped_and_jitterless() {
        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_millis(50), 10);
        assert_eq!(policy.delay(1, None), Duration::from_millis(10));
        assert_eq!(policy.delay(2, None), Duration::from_millis(20));
        assert_eq!(policy.delay(3, None), Duration::from_millis(40));
        assert_eq!(policy.delay(4, None), Duration::from_millis(50));
    }

    #[test]
    fn retry_after_wins_and_is_capped() {
        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_millis(50), 10);
        assert_eq!(
            policy.delay(2, Some(Duration::from_millis(100))),
            Duration::from_millis(100)
        );
        // The default cap is one minute: a one-hour hint is clamped, not slept.
        assert_eq!(
            policy.delay(2, Some(Duration::from_secs(3_600))),
            Duration::from_secs(60)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("0.05"), Some(Duration::from_millis(50)));
    }

    /// Test A: `Retry-After: 28` must produce a delay of at least 28 s, and the
    /// jittered default policy must never undercut the server's instruction.
    #[test]
    fn retry_after_28_seconds_is_honored_in_full() {
        let hint = parse_retry_after("28").expect("delta-seconds form parses");
        assert_eq!(hint, Duration::from_secs(28));

        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_millis(50), 10);
        assert_eq!(policy.delay(1, Some(hint)), Duration::from_secs(28));
        assert_eq!(policy.delay(9, Some(hint)), Duration::from_secs(28));

        // The shipped policy jitters its own backoff; the hint is a floor there
        // too, so no roll can bring the wait below 28 s.
        let jittered = RetryPolicy::default();
        for _ in 0..200 {
            assert!(
                jittered.delay(1, Some(hint)) >= Duration::from_secs(28),
                "jitter shortened a server hint"
            );
        }
    }

    /// Test B: without the header the delay is today's exponential backoff.
    #[test]
    fn missing_retry_after_falls_back_to_exponential() {
        assert_eq!(parse_retry_after(""), None);
        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_secs(10), 10);
        for (attempt, expected) in [(1, 10), (2, 20), (3, 40), (4, 80), (5, 160)] {
            assert_eq!(
                policy.delay(attempt, None),
                Duration::from_millis(expected),
                "attempt {attempt}"
            );
        }
        // Exponential growth stops at `max`.
        assert_eq!(policy.delay(30, None), Duration::from_secs(10));
    }

    /// Test C: a malformed or out-of-range hint is ignored, never a zero delay
    /// and never a panic.
    #[test]
    fn malformed_retry_after_is_ignored() {
        for value in [
            "banana",
            "",
            "   ",
            "-1",
            "-0.5",
            "NaN",
            "inf",
            "-inf",
            "28 seconds",
            "Sun, 06 Nov 1994",
            "Thu, 32 Jan 2099 00:00:00 GMT",
            "Thu, 01 Jan 2099 25:00:00 GMT",
        ] {
            assert_eq!(parse_retry_after(value), None, "value {value:?}");
        }
        // A finite-but-huge delta used to panic inside `Duration::from_secs_f64`.
        // `try_from_secs_f64` reports it as an error, so the hint is ignored.
        assert_eq!(parse_retry_after("1e300"), None);
        // An ignored hint leaves the exponential fallback intact.
        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_millis(50), 10);
        let ignored = parse_retry_after("banana");
        assert_eq!(policy.delay(2, ignored), Duration::from_millis(20));
    }

    /// Test D: an absurd hint is clamped to `max_retry_after`, not slept and not
    /// allowed to overflow.
    #[test]
    fn absurd_retry_after_is_clamped() {
        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_millis(50), 10);
        let hint = parse_retry_after("100000").expect("100000 s parses");
        assert_eq!(hint, Duration::from_secs(100_000));
        assert_eq!(policy.delay(1, Some(hint)), Duration::from_secs(60));

        // A 100000 s hint is beyond the retryable window (error.rs treats a
        // >60 s 429 hint as "do not retry"), so verify the delay bound directly
        // for a status that is always retryable.
        assert_eq!(
            policy.delay(1, Some(Duration::from_secs(100_000))),
            Duration::from_secs(60)
        );
        assert!(policy.delay(1, Some(Duration::MAX)) <= Duration::from_secs(60));
    }

    /// Test E: the HTTP-date form parses and yields a bounded positive delay; a
    /// date in the past means "retry now".
    #[test]
    fn http_date_form_is_bounded() {
        let future =
            parse_retry_after("Thu, 01 Jan 2099 00:00:00 GMT").expect("IMF-fixdate parses");
        assert!(future > Duration::from_secs(60), "{future:?}");
        let policy = RetryPolicy::fixed(Duration::from_millis(10), Duration::from_millis(50), 10);
        assert_eq!(policy.delay(1, Some(future)), Duration::from_secs(60));

        // Past date: zero, i.e. retry immediately; never a negative duration.
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Duration::ZERO)
        );
        assert!(
            parse_retry_after("Fri, 01 Jan 2100 00:00:00 GMT")
                .is_some_and(|delay| delay > Duration::ZERO)
        );
    }

    #[test]
    fn attempts_are_bounded() {
        let mut state = RetryState::new(RetryPolicy::fixed(
            Duration::from_millis(1),
            Duration::from_millis(1),
            3,
        ));
        let error = LlmError::Http {
            status: 500,
            message: "boom".to_string(),
            retry_after_ms: None,
        };
        assert!(state.next_delay(&error).is_some());
        assert!(state.next_delay(&error).is_some());
        assert!(state.next_delay(&error).is_none());
    }
}
