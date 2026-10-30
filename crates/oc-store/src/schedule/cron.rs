//! Five-field cron parsing and the next-slot search in a fixed-offset zone.

use super::zone::civil_from_days;
use super::{CRON_HORIZON_DAYS, MINUTE_MS, MINUTES_PER_DAY};
use crate::error::StoreError;

/// A parsed five-field cron expression, evaluated in a fixed-offset zone.
struct CronSpec {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    days_any: bool,
    weekdays_any: bool,
}

impl CronSpec {
    fn parse(expression: &str) -> Result<Self, StoreError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(invalid_cron(expression));
        }
        let minutes = cron_field(fields[0], 0, 59, expression)?;
        let hours = cron_field(fields[1], 0, 23, expression)?;
        let days = cron_field(fields[2], 1, 31, expression)?;
        let months = cron_field(fields[3], 1, 12, expression)?;
        let weekdays = normalize_weekdays(cron_field(fields[4], 0, 7, expression)?);
        let days_any = days.len() == 31;
        let weekdays_any = weekdays.len() == 7;
        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays,
            days_any,
            weekdays_any,
        })
    }
}

fn invalid_cron(expression: &str) -> StoreError {
    StoreError::Invalid(format!("invalid cron: {expression:?}"))
}

/// Parses one cron field into sorted unique values.
fn cron_field(field: &str, min: u32, max: u32, expression: &str) -> Result<Vec<u32>, StoreError> {
    if field.is_empty() {
        return Err(invalid_cron(expression));
    }
    let mut values = Vec::new();
    for part in field.split(',') {
        if part.is_empty() {
            return Err(invalid_cron(expression));
        }
        if part == "*" {
            values.extend(min..=max);
        } else if let Some(step) = part.strip_prefix("*/") {
            let step: u32 = step.parse().map_err(|_| invalid_cron(expression))?;
            let step = usize::try_from(step).map_err(|_| invalid_cron(expression))?;
            if step == 0 {
                return Err(invalid_cron(expression));
            }
            values.extend((min..=max).step_by(step));
        } else if let Some((start, end)) = part.split_once('-') {
            let start: u32 = start.parse().map_err(|_| invalid_cron(expression))?;
            let end: u32 = end.parse().map_err(|_| invalid_cron(expression))?;
            if start < min || end > max || start > end {
                return Err(invalid_cron(expression));
            }
            values.extend(start..=end);
        } else {
            let value: u32 = part.parse().map_err(|_| invalid_cron(expression))?;
            if !(min..=max).contains(&value) {
                return Err(invalid_cron(expression));
            }
            values.push(value);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

/// Maps cron's `7` to Sunday and sorts the result.
fn normalize_weekdays(mut values: Vec<u32>) -> Vec<u32> {
    for value in &mut values {
        if *value == 7 {
            *value = 0;
        }
    }
    values.sort_unstable();
    values.dedup();
    values
}

/// The first cron slot strictly after `now_ms`, in the fixed-offset zone.
pub(super) fn next_cron_after(
    expression: &str,
    offset_ms: i64,
    now_ms: i64,
) -> Result<Option<i64>, StoreError> {
    let spec = CronSpec::parse(expression)?;
    let local_now = now_ms
        .checked_add(offset_ms)
        .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))?;
    let first_minute = local_now.div_euclid(MINUTE_MS) + 1;
    let mut day = first_minute.div_euclid(MINUTES_PER_DAY);
    let last_day = day + CRON_HORIZON_DAYS;
    while day <= last_day {
        let (year, month, day_of_month) = civil_from_days(day);
        if year > 9_999 {
            return Ok(None);
        }
        let weekday = u32::try_from((day + 4).rem_euclid(7)).unwrap_or_default();
        let day_matches = spec.days.contains(&day_of_month);
        let weekday_matches = spec.weekdays.contains(&weekday);
        let date_matches = match (spec.days_any, spec.weekdays_any) {
            (true, true) => true,
            (true, false) => weekday_matches,
            (false, true) => day_matches,
            (false, false) => day_matches || weekday_matches,
        };
        if spec.months.contains(&month) && date_matches {
            for hour in &spec.hours {
                for minute in &spec.minutes {
                    let candidate =
                        day * MINUTES_PER_DAY + i64::from(*hour) * 60 + i64::from(*minute);
                    if candidate >= first_minute {
                        let local = candidate.checked_mul(MINUTE_MS).ok_or_else(|| {
                            StoreError::Invalid("schedule time overflows".to_owned())
                        })?;
                        return Ok(Some(local - offset_ms));
                    }
                }
            }
        }
        day += 1;
    }
    Ok(None)
}
