//! Experimental settings normalization.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;
use super::keys::{CONSUMED_EXPERIMENTAL, DROPPED_EXPERIMENTAL};
use super::mcp::positive;

pub(super) fn normalize_experimental(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let Some(value) = input.get("experimental") else {
        return;
    };
    let Some(object) = value.as_object() else {
        ctx.invalid("$.experimental");
        return;
    };
    let mut result = Map::new();
    for key in DROPPED_EXPERIMENTAL {
        if object.contains_key(key) {
            ctx.unsupported(
                &jp("$.experimental", key),
                "omitted unsupported legacy setting",
            );
        }
    }
    for key in [
        "portable_shell_scanner",
        "todo",
        "background_compaction",
        "media_blob",
        "image_generate",
        "schedules",
    ] {
        if let Some(value) = object.get(key) {
            match value.as_bool() {
                Some(value) => {
                    result.insert(key.to_owned(), Value::Bool(value));
                }
                None => ctx.invalid(&jp("$.experimental", key)),
            }
        }
    }
    for key in ["subagent_depth", "location_idle_timeout"] {
        if let Some(value) = object.get(key) {
            match positive(value) {
                Some(value) => {
                    result.insert(key.to_owned(), Value::from(value));
                }
                None => ctx.invalid(&jp("$.experimental", key)),
            }
        }
    }
    // Unknown experimental keys survive; consumed keys do not.
    for (key, value) in object {
        if result.contains_key(key)
            || CONSUMED_EXPERIMENTAL.contains(&key.as_str())
            || DROPPED_EXPERIMENTAL.contains(&key.as_str())
        {
            continue;
        }
        result.insert(key.clone(), value.clone());
    }
    if !result.is_empty() || object.is_empty() {
        out.insert("experimental".to_owned(), Value::Object(result));
    }
}
