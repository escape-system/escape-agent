//! Provider retry policy.
//!
//! Exponential 2 s → 10 s per gap, 10 attempts, optional jitter, and a
//! provider-requested `retry-after` capped at 15 minutes. Timeout retries are
//! counted separately because a timed-out attempt has already waited.

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
            max_retry_after: Duration::from_secs(15 * 60),
            max_timeout_retries: 3,
        }
    }
}

impl RetryPolicy {
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
    #[must_use]
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exponent = attempt.saturating_sub(1).min(20);
        let factor = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let base = self.base.saturating_mul(factor).min(self.max);
        let mut delay = if self.jitter {
            // ±25% keeps the exponential shape while spreading retries.
            let roll: f64 = rand::random();
            let scale = 0.75 + roll * 0.5;
            base.mul_f64(scale)
        } else {
            base
        };
        if let Some(retry_after) = retry_after {
            delay = delay.max(retry_after.min(self.max_retry_after));
        }
        delay.min(self.max.max(self.max_retry_after))
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

/// Parse a `retry-after` header value. Supports delta seconds (including
/// fractional values used by test servers) and bare milliseconds are not valid
/// HTTP, so only seconds are accepted.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        return Some(Duration::from_secs_f64(seconds));
    }
    parse_http_date(value)
}

/// IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`. A date in the past means
/// "retry immediately" (zero delay).
fn parse_http_date(value: &str) -> Option<Duration> {
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
    Some(Duration::from_secs(
        u64::try_from(seconds.saturating_sub(now)).unwrap_or(0),
    ))
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
        assert_eq!(
            policy.delay(2, Some(Duration::from_secs(3_600))),
            Duration::from_secs(900)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("0.05"), Some(Duration::from_millis(50)));
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
