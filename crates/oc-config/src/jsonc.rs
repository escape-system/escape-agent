//! Strict JSONC parsing: comments and trailing commas are allowed, JSON5 extensions are not.

use serde_json::Value;

/// Parse strict JSONC: comments and trailing commas are allowed, JSON5
/// extensions are not.
pub(crate) fn parse_jsonc(text: &str) -> Result<Value, String> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    };
    jsonc_parser::parse_to_serde_value::<Value>(text, &options).map_err(|error| error.to_string())
}
