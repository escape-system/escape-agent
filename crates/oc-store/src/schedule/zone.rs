//! Fixed-offset zone parsing, RFC 3339 instants and proleptic Gregorian date math.

use super::{DAY_MS, HOUR_MS, MINUTE_MS};
use crate::error::StoreError;

/// Canonical zone string and its fixed offset in milliseconds.
///
/// Accepts `UTC` (the default) and `+HH:MM` / `-HH:MM`. Everything else is an
/// error: this store has no IANA database.
pub(super) fn zone_offset_ms(tz: Option<&str>) -> Result<(String, i64), StoreError> {
    let raw = match tz {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(StoreError::Invalid(format!(
                    "unsupported time zone: {value:?}"
                )));
            }
            value
        }
        None => "UTC",
    };
    if raw.eq_ignore_ascii_case("utc") || raw.eq_ignore_ascii_case("z") {
        return Ok(("UTC".to_owned(), 0));
    }
    let (sign, rest) = match raw.as_bytes().first() {
        Some(b'+') => (1_i64, &raw[1..]),
        Some(b'-') => (-1_i64, &raw[1..]),
        _ => {
            return Err(StoreError::Invalid(format!(
                "unsupported time zone: {raw:?}"
            )));
        }
    };
    let Some((hours, minutes)) = rest.split_once(':') else {
        return Err(StoreError::Invalid(format!(
            "unsupported time zone: {raw:?}"
        )));
    };
    if hours.len() != 2
        || minutes.len() != 2
        || !hours.bytes().all(|byte| byte.is_ascii_digit())
        || !minutes.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(StoreError::Invalid(format!(
            "unsupported time zone: {raw:?}"
        )));
    }
    let hours: i64 = hours
        .parse()
        .map_err(|_| StoreError::Invalid(format!("unsupported time zone: {raw:?}")))?;
    let minutes: i64 = minutes
        .parse()
        .map_err(|_| StoreError::Invalid(format!("unsupported time zone: {raw:?}")))?;
    if hours > 23 || minutes > 59 {
        return Err(StoreError::Invalid(format!(
            "unsupported time zone: {raw:?}"
        )));
    }
    let mark = if sign > 0 { '+' } else { '-' };
    Ok((
        format!("{mark}{hours:02}:{minutes:02}"),
        sign * (hours * HOUR_MS + minutes * MINUTE_MS),
    ))
}

/// Parses an RFC 3339 instant, a wall-clock instant in `default_offset` when no
/// offset is written, or a numeric epoch-millisecond value.
pub(super) fn parse_instant(value: &str, default_offset: i64) -> Result<i64, StoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_instant(value));
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    if !unsigned.is_empty() && unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<i64>().map_err(|_| invalid_instant(value));
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    let year = take_digits(bytes, &mut index, 4).ok_or_else(|| invalid_instant(value))?;
    expect(bytes, &mut index, b'-').ok_or_else(|| invalid_instant(value))?;
    let month = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    expect(bytes, &mut index, b'-').ok_or_else(|| invalid_instant(value))?;
    let day = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    if !matches!(bytes.get(index), Some(b'T' | b't' | b' ')) {
        return Err(invalid_instant(value));
    }
    index += 1;
    let hour = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    expect(bytes, &mut index, b':').ok_or_else(|| invalid_instant(value))?;
    let minute = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    let mut second = 0;
    let mut fraction = 0;
    if bytes.get(index) == Some(&b':') {
        index += 1;
        second = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            let digits = &bytes[start..index];
            if digits.is_empty() {
                return Err(invalid_instant(value));
            }
            let mut millis = 0_i64;
            for (position, digit) in digits.iter().take(3).enumerate() {
                millis += i64::from(*digit - b'0') * [100_i64, 10, 1][position];
            }
            fraction = millis;
        }
    }
    let offset = if index == bytes.len() {
        default_offset
    } else {
        match bytes[index] {
            b'Z' | b'z' => {
                index += 1;
                0
            }
            sign @ (b'+' | b'-') => {
                index += 1;
                let hours =
                    take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
                expect(bytes, &mut index, b':').ok_or_else(|| invalid_instant(value))?;
                let minutes =
                    take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
                if hours > 23 || minutes > 59 {
                    return Err(invalid_instant(value));
                }
                let magnitude = hours * HOUR_MS + minutes * MINUTE_MS;
                if sign == b'-' { -magnitude } else { magnitude }
            }
            _ => return Err(invalid_instant(value)),
        }
    };
    if index != bytes.len() {
        return Err(invalid_instant(value));
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > i64::from(days_in_month(year, u32::try_from(month).unwrap_or(1)))
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid_instant(value));
    }
    let days = days_from_civil(
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    );
    let local = days
        .checked_mul(DAY_MS)
        .and_then(|value| {
            value.checked_add(hour * HOUR_MS + minute * MINUTE_MS + second * 1_000 + fraction)
        })
        .ok_or_else(|| invalid_instant(value))?;
    local
        .checked_sub(offset)
        .ok_or_else(|| invalid_instant(value))
}

fn invalid_instant(value: &str) -> StoreError {
    StoreError::Invalid(format!("invalid time: {value:?}"))
}

fn take_digits(bytes: &[u8], index: &mut usize, count: usize) -> Option<i64> {
    let end = index.checked_add(count)?;
    let slice = bytes.get(*index..end)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *index = end;
    Some(
        slice
            .iter()
            .fold(0_i64, |value, byte| value * 10 + i64::from(*byte - b'0')),
    )
}

fn expect(bytes: &[u8], index: &mut usize, expected: u8) -> Option<()> {
    if bytes.get(*index) == Some(&expected) {
        *index += 1;
        Some(())
    } else {
        None
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let day = i64::from(day);
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date for a day count since 1970-01-01.
pub(super) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}
