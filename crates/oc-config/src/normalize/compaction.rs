//! Compaction settings normalization.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;

pub(super) fn normalize_compaction(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let Some(value) = input.get("compaction") else {
        return;
    };
    let Some(object) = value.as_object() else {
        ctx.invalid("$.compaction");
        return;
    };
    for key in ["tail_turns", "prune"] {
        if object.contains_key(key) {
            ctx.unsupported(
                &jp("$.compaction", key),
                "omitted unsupported legacy setting",
            );
        }
    }
    let mut result = Map::new();
    if let Some(value) = object.get("auto") {
        match value.as_bool() {
            Some(value) => {
                result.insert("auto".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid("$.compaction.auto"),
        }
    }
    let mut keep_tokens = None;
    if let Some(keep) = object.get("keep").and_then(Value::as_object)
        && let Some(value) = keep.get("tokens")
    {
        match value.as_u64() {
            Some(value) => keep_tokens = Some(value),
            None => ctx.invalid("$.compaction.keep.tokens"),
        }
    }
    if let Some(value) = object.get("preserve_recent_tokens") {
        match value.as_u64() {
            Some(value) => keep_tokens = Some(value),
            None => ctx.invalid("$.compaction.preserve_recent_tokens"),
        }
    }
    if let Some(tokens) = keep_tokens {
        result.insert("keep".to_owned(), serde_json::json!({ "tokens": tokens }));
    }
    let buffer = object.get("buffer").or_else(|| object.get("reserved"));
    if let Some(buffer) = buffer {
        match buffer.as_u64() {
            Some(value) => {
                result.insert("buffer".to_owned(), Value::from(value));
            }
            None => ctx.invalid("$.compaction.buffer"),
        }
    }
    if !result.is_empty() || object.is_empty() {
        out.insert("compaction".to_owned(), Value::Object(result));
    }
}
