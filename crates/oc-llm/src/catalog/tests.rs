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
