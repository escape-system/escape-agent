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

pub(super) fn number_at(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(json_i64).unwrap_or(0)
}

pub(super) fn float_at(value: &Value, pointer: &str) -> f64 {
    value
        .pointer(pointer)
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

pub(super) fn empty_tokens() -> Value {
    json!({ "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } })
}

pub(super) fn add_tokens(total: &mut Value, message: &Value) {
    for (target, source) in [
        ("/input", "/tokens/input"),
        ("/output", "/tokens/output"),
        ("/reasoning", "/tokens/reasoning"),
        ("/cache/read", "/tokens/cache/read"),
        ("/cache/write", "/tokens/cache/write"),
    ] {
        let current = number_at(total, target);
        let add = number_at(message, source);
        if let Some(slot) = total.pointer_mut(target) {
            *slot = json!(current + add);
        }
    }
}

pub(super) fn tokens_total(tokens: &Value) -> i64 {
    number_at(tokens, "/input")
        + number_at(tokens, "/output")
        + number_at(tokens, "/reasoning")
        + number_at(tokens, "/cache/read")
        + number_at(tokens, "/cache/write")
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
