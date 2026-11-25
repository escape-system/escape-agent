//! Token, tool-duration and calendar arithmetic for the stats endpoint.

use oc_types::ApiError;
use serde_json::{Value, json};

use super::super::common::{float_i64, json_i64};

pub(super) fn parse_epoch(raw: &str, field: &str) -> Result<i64, ApiError> {
    if let Ok(value) = raw.parse::<i64>() {
        return Ok(value);
    }
    if let Ok(value) = raw.parse::<f64>()
        && let Some(value) = float_i64(value)
    {
        return Ok(value);
    }
    Err(ApiError::new(
        400,
        "InvalidRequestError",
        format!("{field} must be an epoch in milliseconds"),
    ))
}

pub(super) fn float_at(value: &Value, pointer: &str) -> f64 {
    value
        .pointer(pointer)
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

#[derive(Default, Clone, Copy)]
pub(super) struct TokenCounts {
    pub(super) input: i64,
    pub(super) output: i64,
    pub(super) reasoning: i64,
    pub(super) cache_read: i64,
    pub(super) cache_write: i64,
}

impl TokenCounts {
    pub(super) fn from_message(message: &Value) -> Self {
        let Some(tokens) = message.get("tokens") else {
            return Self::default();
        };
        let cache = tokens.get("cache");
        Self {
            input: tokens.get("input").and_then(json_i64).unwrap_or(0),
            output: tokens.get("output").and_then(json_i64).unwrap_or(0),
            reasoning: tokens.get("reasoning").and_then(json_i64).unwrap_or(0),
            cache_read: cache
                .and_then(|c| c.get("read"))
                .and_then(json_i64)
                .unwrap_or(0),
            cache_write: cache
                .and_then(|c| c.get("write"))
                .and_then(json_i64)
                .unwrap_or(0),
        }
    }

    pub(super) fn add_assign(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    pub(super) fn total(self) -> i64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }

    pub(super) fn to_json(self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "reasoning": self.reasoning,
            "cache": { "read": self.cache_read, "write": self.cache_write }
        })
    }
}

/// Days since the Unix epoch to a civil `(year, month, day)` (Hinnant).
pub(super) fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub(super) fn day_ordinal(millis: i64) -> i64 {
    millis.div_euclid(86_400_000)
}

pub(super) fn date_key(millis: i64) -> String {
    let (year, month, day) = civil_from_days(day_ordinal(millis));
    format!("{year:04}-{month:02}-{day:02}")
}

pub(super) fn median(values: &mut [u32]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::from(values[middle - 1]).midpoint(f64::from(values[middle]))
    } else {
        f64::from(values[middle])
    }
}

pub(super) fn day_ordinal_from_key(date: &str) -> i64 {
    let mut parts = date.split('-');
    let year: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let month: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    let day: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    days_from_civil(year, month, day)
}

/// Inverse of [`civil_from_days`].
pub(super) fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
