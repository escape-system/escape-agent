//! Trigger parsing: durations and the `Schedule.When` variants, with first-due and next-slot math.

use serde::{Deserialize, Serialize};

use super::cron::next_cron_after;
use super::repo::add;
use super::zone::{parse_instant, zone_offset_ms};
use super::{DAY_MS, HOUR_MS, MINUTE_MS};
use crate::error::StoreError;

/// Repeating schedules may not fire faster than once a minute.
const MIN_INTERVAL_MS: i64 = MINUTE_MS;

/// A parsed duration in milliseconds.
///
/// Accepts a plain integer (seconds) or one or more `<integer> <unit>` pairs,
/// with `ms`, seconds, minutes, hours, days and weeks:
/// `"90 seconds"`, `"5 minutes"`, `"2 hours"`, `"1 day"`, `"1 hour 30 minutes"`.
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] for a missing, negative, zero or overflowing
/// duration, or an unknown unit.
pub fn duration_ms(value: &str) -> Result<i64, StoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_duration(value));
    }
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() == 1 {
        let seconds: i64 = parts[0].parse().map_err(|_| invalid_duration(value))?;
        return positive(
            seconds
                .checked_mul(1_000)
                .ok_or_else(|| invalid_duration(value))?,
            value,
        );
    }
    let mut total = 0_i64;
    for pair in parts.chunks(2) {
        if pair.len() != 2 {
            return Err(invalid_duration(value));
        }
        let amount: i64 = pair[0].parse().map_err(|_| invalid_duration(value))?;
        let unit = pair[1];
        if amount <= 0 {
            return Err(invalid_duration(value));
        }
        let unit_ms = match unit.to_ascii_lowercase().as_str() {
            "ms" | "millisecond" | "milliseconds" => 1,
            "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
            "m" | "min" | "mins" | "minute" | "minutes" => MINUTE_MS,
            "h" | "hr" | "hrs" | "hour" | "hours" => HOUR_MS,
            "d" | "day" | "days" => DAY_MS,
            "w" | "week" | "weeks" => 7 * DAY_MS,
            _ => return Err(invalid_duration(value)),
        };
        let part = amount
            .checked_mul(unit_ms)
            .ok_or_else(|| invalid_duration(value))?;
        total = total
            .checked_add(part)
            .ok_or_else(|| invalid_duration(value))?;
    }
    positive(total, value)
}

fn positive(millis: i64, value: &str) -> Result<i64, StoreError> {
    if millis > 0 {
        Ok(millis)
    } else {
        Err(invalid_duration(value))
    }
}

fn invalid_duration(value: &str) -> StoreError {
    StoreError::Invalid(format!("invalid duration: {value:?}"))
}

/// The wire `Schedule.When` object.
///
/// Serde uses the tag `type` and lower-case variant names, so this round-trips
/// as `{"type":"in","in":"5 minutes"}`,
/// `{"type":"at","at":"2026-10-27T09:00:00Z"}`,
/// `{"type":"every","every":"2 hours"}` or
/// `{"type":"cron","cron":"*/5 * * * *"}`, each with an optional `tz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScheduleWhen {
    /// One-shot: the duration from creation.
    In {
        #[serde(rename = "in")]
        r#in: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// One-shot: an RFC 3339 instant or an epoch-millisecond number.
    At {
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Repeating: the fixed interval.
    Every {
        every: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Repeating: a five-field cron expression (`minute hour day month weekday`).
    Cron {
        cron: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
}

impl ScheduleWhen {
    /// The zone supplied with the trigger, if any.
    #[must_use]
    pub fn tz(&self) -> Option<&str> {
        match self {
            Self::In { tz, .. }
            | Self::At { tz, .. }
            | Self::Every { tz, .. }
            | Self::Cron { tz, .. } => tz.as_deref(),
        }
    }

    /// The first instant this trigger fires, strictly after `now_ms`.
    ///
    /// `in` and `every` are relative to `now_ms`; `at` must be in the future;
    /// `cron` is evaluated in the trigger's zone (UTC when absent). The zone is
    /// validated for every variant, including the relative triggers whose math
    /// does not use it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a malformed duration, instant, cron
    /// expression or zone, an `at` in the past, an `every` below one minute, or
    /// a cron expression that never matches.
    pub fn first_due(&self, now_ms: i64) -> Result<i64, StoreError> {
        match self {
            Self::In { r#in, .. } => {
                // A relative trigger has no wall-clock component, but an
                // explicit zone is still validated rather than ignored.
                self.zone()?;
                add(now_ms, duration_ms(r#in)?)
            }
            Self::At { at, .. } => {
                let (_, offset) = self.zone()?;
                let instant = parse_instant(at, offset)?;
                if instant <= now_ms {
                    return Err(StoreError::Invalid(format!("time is in the past: {at:?}")));
                }
                Ok(instant)
            }
            Self::Every { every, .. } => {
                self.zone()?;
                let every = duration_ms(every)?;
                if every < MIN_INTERVAL_MS {
                    return Err(StoreError::Invalid(
                        "every must be at least 1 minute".to_owned(),
                    ));
                }
                add(now_ms, every)
            }
            Self::Cron { cron, .. } => {
                let (_, offset) = self.zone()?;
                next_cron_after(cron, offset, now_ms)?
                    .ok_or_else(|| StoreError::Invalid(format!("cron never fires: {cron:?}")))
            }
        }
    }

    /// The next firing strictly after `now_ms`, given the previous due instant.
    ///
    /// Returns `None` for the one-shot `in` and `at` triggers. `every` keeps the
    /// phase from `previous_due` and skips slots already missed; `cron` ignores
    /// `previous_due`. The zone is validated for every variant.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a malformed duration, cron expression or
    /// zone.
    pub fn next_after(&self, now_ms: i64, previous_due: i64) -> Result<Option<i64>, StoreError> {
        match self {
            Self::In { .. } | Self::At { .. } => {
                self.zone()?;
                Ok(None)
            }
            Self::Every { every, .. } => {
                self.zone()?;
                let every = duration_ms(every)?;
                if previous_due > now_ms {
                    return Ok(Some(previous_due));
                }
                let elapsed = now_ms
                    .checked_sub(previous_due)
                    .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))?;
                let missed = elapsed.div_euclid(every) + 1;
                let step = missed
                    .checked_mul(every)
                    .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))?;
                Ok(Some(add(previous_due, step)?))
            }
            Self::Cron { cron, .. } => {
                let (_, offset) = self.zone()?;
                next_cron_after(cron, offset, now_ms)
            }
        }
    }

    /// Replaces the zone with the resolved canonical form.
    #[must_use]
    pub(super) fn with_tz(self, tz: &str) -> Self {
        let zone = Some(tz.to_owned());
        match self {
            Self::In { r#in, .. } => Self::In { r#in, tz: zone },
            Self::At { at, .. } => Self::At { at, tz: zone },
            Self::Every { every, .. } => Self::Every { every, tz: zone },
            Self::Cron { cron, .. } => Self::Cron { cron, tz: zone },
        }
    }

    pub(super) fn zone(&self) -> Result<(String, i64), StoreError> {
        zone_offset_ms(self.tz())
    }
}
