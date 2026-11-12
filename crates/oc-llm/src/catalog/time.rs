//! Release date parsing and civil-date math.

/// Highest epoch-millisecond value accepted from a **numeric** release date:
/// 2100-01-01.
///
/// A floating-point value cast with `as i64` saturates instead of failing, so a
/// bogus `"1e308"` release date would sort a model as newest forever. Numeric
/// input is clamped to this window and returns the `0` sentinel outside it.
///
/// The `YYYY-MM-DD` path shares the same accepted year ([`MAX_YEAR`]) but not
/// this exact instant: a numeric value carries no calendar, so its window ends
/// at the first day of the last year, while a written date may end at
/// `2100-12-31`. Both paths use the `0` sentinel outside their window.
///
/// Held as `f64` because the only comparison is against a parsed `f64`; the
/// literal is exactly representable (it is below 2^53).
const MAX_RELEASE_MS: f64 = 4_102_444_800_000.0;

/// Last calendar year accepted by the `YYYY-MM-DD` path: 2100-12-31.
///
/// Must stay in step with [`MAX_RELEASE_MS`], which is `2100-01-01`; the two
/// years are asserted equal by
/// `catalog::tests::numeric_and_calendar_windows_agree`.
const MAX_YEAR: i64 = 2100;

/// First calendar year accepted by the `YYYY-MM-DD` path.
///
/// `0001-01-01` precedes the epoch, so its value is negative; it is still a
/// well-defined timestamp and used to be accepted, so it stays accepted.
const MIN_YEAR: i64 = 1;

/// Milliseconds in a day; the single place the date path scales days.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// Numeric values below this are epoch **seconds**; at or above it they are
/// already milliseconds.
///
/// 1e11 ms is 1973-03-03 while 1e11 s is year 5138, so no plausible release
/// timestamp sits near the boundary. Treating small values as seconds keeps the
/// models.dev convention working, and treating large ones as milliseconds stops
/// a millisecond input from being multiplied twice (which lands in year ~55000).
const SECONDS_UPPER_BOUND: f64 = 1e11;

/// Parse `YYYY-MM-DD` / `YYYY-MM` / RFC 3339 into epoch milliseconds. Invalid input is `0`.
///
/// A written date is accepted only when its year is in [`MIN_YEAR..=MAX_YEAR`]
/// (`0001..=2100`) and its month/day are within `1..=12` / `1..=31`; anything
/// else — including a year too large to represent as milliseconds — is the `0`
/// sentinel. This is a rejection window, not calendar validation: `2024-02-31`
/// is accepted as if it were `2024-03-02`, exactly as before.
///
/// Numeric input is interpreted as follows:
/// - a bare integer in `1000..=9999` is a calendar year (`"2025"` -> `2025-01-01`),
///   matching how abbreviated release dates are written;
/// - below [`SECONDS_UPPER_BOUND`] it is epoch seconds (`"1759104000"`);
/// - at or above it it is epoch milliseconds (`"1759104000000"`), not scaled again.
///
/// Non-finite values (`NaN`, `inf`), negatives and numeric values outside
/// `1970-01-01..=2100-01-01` are rejected as `0`, the same sentinel used for
/// unparseable input.
#[must_use]
pub fn parse_release_date(value: &str) -> i64 {
    if let Ok(number) = value.parse::<f64>() {
        return parse_numeric(number);
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
    if !(MIN_YEAR..=MAX_YEAR).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return 0;
    }
    days_from_civil(year, month, day).saturating_mul(MILLIS_PER_DAY)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "f64 is checked finite and clamped to MAX_RELEASE_MS (2^42) before the cast"
)]
fn parse_numeric(number: f64) -> i64 {
    if !number.is_finite() || number < 0.0 {
        return 0;
    }
    // A bare year such as "2025" is a calendar year, not 33 minutes past epoch.
    if number.fract() == 0.0 && (1000.0..=9999.0).contains(&number) {
        return days_from_civil(number as i64, 1, 1).saturating_mul(MILLIS_PER_DAY);
    }
    let millis = if number < SECONDS_UPPER_BOUND {
        number * 1000.0
    } else {
        number
    };
    if !millis.is_finite() || millis > MAX_RELEASE_MS {
        return 0;
    }
    millis as i64
}

// Howard Hinnant's days_from_civil.
//
// Precondition: `1..=9999` year, `1..=12` month, `1..=31` day. Both callers
// enforce it; the intermediate `era * 146_097` and the caller's
// `days * MILLIS_PER_DAY` both overflow well before `i64` exponents otherwise
// (a year beyond ~2.9e8 already overflows the millisecond product).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    debug_assert!(
        (1..=9_999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day),
        "days_from_civil out of range: {year}-{month}-{day}"
    );
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
