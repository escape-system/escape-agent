//! Catalog unit tests.

use super::*;

#[test]
fn reference_rejects_bad_syntax() {
    for reference in [
        "",
        "provider",
        "/model",
        "provider/",
        "provider/#x",
        "pro#vider/x",
    ] {
        assert!(parse_reference(reference).is_err(), "{reference}");
    }
    assert_eq!(
        parse_reference("anthropic/claude#high").unwrap(),
        ModelRef {
            provider: "anthropic".to_string(),
            id: "claude".to_string(),
            variant: Some("high".to_string()),
        }
    );
}

#[test]
fn date_parses_to_millis() {
    assert_eq!(parse_release_date("1970-01-01"), 0);
    assert_eq!(parse_release_date("2025-09-29"), 1_759_104_000_000);
}

#[test]
fn numeric_release_dates_never_saturate() {
    // `f64 as i64` used to saturate: i64::MAX sorts as newest forever.
    assert_eq!(parse_release_date("1e308"), 0);
    assert_eq!(parse_release_date("-1e308"), 0);
    assert_eq!(parse_release_date("NaN"), 0);
    assert_eq!(parse_release_date("inf"), 0);
    assert_eq!(parse_release_date("-inf"), 0);
    // 5e12 ms is year 2128: outside the plausible window.
    assert_eq!(parse_release_date("5000000000000"), 0);
    // Negative epoch offsets are rejected, not returned as negative dates.
    assert_eq!(parse_release_date("-1759104000"), 0);
}

#[test]
fn numeric_release_dates_use_the_right_unit() {
    let expected = 1_759_104_000_000;
    // Epoch seconds are scaled exactly once...
    assert_eq!(parse_release_date("1759104000"), expected);
    assert_eq!(parse_release_date("1.759104e9"), expected);
    // ...and a value that is already milliseconds is not scaled again.
    assert_eq!(parse_release_date("1759104000000"), expected);
}

#[test]
fn calendar_dates_never_overflow() {
    // `days_from_civil(year, ..) * 86_400_000` used to overflow for any year the
    // registry (or a user overlay) supplied: debug panicked, release wrapped into
    // a negative or arbitrary timestamp.
    assert_eq!(parse_release_date("999999999999999999-01-01"), 0);
    // This one overflows inside `days_from_civil` itself (`era * 146_097`).
    assert_eq!(parse_release_date("9223372036854775807-01-01"), 0);
    assert_eq!(parse_release_date("9223372036854775807-12-31T00:00:00Z"), 0);
}

#[test]
fn calendar_dates_outside_the_window_are_rejected() {
    // One day past the last accepted year, and a year before the first.
    assert_eq!(parse_release_date("2101-01-01"), 0);
    assert_eq!(parse_release_date("0000-12-31"), 0);
    assert_eq!(parse_release_date("-0001-01-01"), 0);
}

#[test]
fn calendar_dates_at_the_window_edges_are_accepted() {
    // `0001-01-01` and `2100-12-31` are the narrowest/latest dates that still fit
    // the window; both are non-zero and must not panic. `0001-01-01` precedes the
    // epoch, so it is negative -- that predates this fix and is preserved.
    assert_eq!(parse_release_date("0001-01-01"), -62_135_596_800_000);
    assert_eq!(parse_release_date("2100-12-31"), 4_133_894_400_000);
    // A value already asserted elsewhere: the refactor must not shift it.
    assert_eq!(parse_release_date("2025-09-29"), 1_759_104_000_000);
}

#[test]
fn numeric_and_calendar_windows_agree() {
    // `MAX_RELEASE_MS` ends the numeric window at 2100-01-01; the calendar path
    // accepts through 2100-12-31. Neither may accept the year after.
    assert_eq!(parse_release_date("4102444800000"), 4_102_444_800_000);
    assert_eq!(parse_release_date("4133980800001"), 0);
    assert!(parse_release_date("2100-01-01") > 0);
    assert!(parse_release_date("2100-12-31") > parse_release_date("2100-01-01"));
    assert_eq!(parse_release_date("2101-01-01"), 0);
}

#[test]
fn calendar_dates_with_bad_month_or_day_are_rejected() {
    for value in [
        "2024-13-01",
        "2024-01-32",
        "2024-00-10",
        "2024-01-00",
        "2024-99-99",
        "2024--1-01",
    ] {
        assert_eq!(parse_release_date(value), 0, "{value}");
    }
}

#[test]
fn calendar_values_are_unchanged_by_the_bounds_fix() {
    // Values cross-checked against an independent civil-date implementation
    // (Python's `datetime`, UTC). The bounds fix must not move any of them.
    for (value, expected) in [
        ("1970-01-01", 0),
        ("1970-01-02", 86_400_000),
        ("1999-12-31", 946_598_400_000),
        ("2000-02-29", 951_782_400_000),
        ("2024-02-29", 1_709_164_800_000),
        ("2024-03-01", 1_709_251_200_000),
        ("2025-01-01", 1_735_689_600_000),
        ("2025-09-29", 1_759_104_000_000),
        ("2099-12-31", 4_102_358_400_000),
        ("2100-02-28", 4_107_456_000_000),
        ("2100-03-01", 4_107_542_400_000),
        ("2100-12-31", 4_133_894_400_000),
    ] {
        assert_eq!(parse_release_date(value), expected, "{value}");
    }
    // Month-precision dates keep defaulting to the first of the month.
    assert_eq!(parse_release_date("2025-09"), 1_756_684_800_000);
}

#[test]
fn hostile_snapshot_release_date_does_not_panic() {
    // Reachability proof: the date comes from a caller-supplied snapshot, not
    // from this crate's own constants. This used to panic while parsing.
    let snapshot = r#"{
      "evil": {
        "id": "evil",
        "name": "Evil",
        "models": {
          "evil-model": {
            "id": "evil-model",
            "type": "chat",
            "name": "Evil",
            "release_date": "999999999999999999-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}},
            "limit": {"context": 1, "output": 1},
            "cost": {"text": {"input": {"price": 1}, "output": {"price": 1}}},
            "api": {
              "chat-completions": {
                "base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
              }
            }
          }
        }
      }
    }"#;
    let catalog = Catalog::from_snapshot(snapshot).expect("snapshot parses");
    let model = catalog.model("evil", "evil-model").expect("model present");
    assert_eq!(model.time.released, 0);
}

#[test]
fn bare_year_is_a_calendar_year() {
    // "2025" used to become 1970-01-01T00:33:45Z (2025 seconds).
    assert_eq!(parse_release_date("2025"), parse_release_date("2025-01-01"));
    assert_eq!(parse_release_date("2025"), 1_735_689_600_000);
    assert_eq!(parse_release_date("1999"), parse_release_date("1999-01-01"));
}
