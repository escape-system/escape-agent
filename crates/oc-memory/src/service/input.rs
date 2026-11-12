//! Input normalization shared by the write path.

/// Trims `value` and falls back to `fallback` when it is absent or blank.
pub(super) fn normalized(value: Option<&str>, fallback: &str) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}
