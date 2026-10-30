//! Release date parsing and civil-date math.

/// Parse `YYYY-MM-DD` / `YYYY-MM` / RFC 3339 into epoch milliseconds. Invalid input is `0`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "numeric release timestamps are milliseconds and bounded by epoch ranges"
)]
pub fn parse_release_date(value: &str) -> i64 {
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds * 1000.0) as i64;
    }
    let date = value.split_once('T').map_or(value, |(date, _)| date).trim();
    let mut parts = date.split('-');
    let (Some(year), Some(month)) = (parts.next(), parts.next()) else {
        return 0;
    };
    let (Ok(year), Ok(month)) = (year.parse::<i64>(), month.parse::<i64>()) else {
        return 0;
    };
    let day = parts
        .next()
        .and_then(|day| day.parse::<i64>().ok())
        .unwrap_or(1);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    days_from_civil(year, month, day) * 86_400_000
}

// Howard Hinnant's days_from_civil.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
