//! Shared copy helpers that carry recognized values into the output document.

use serde_json::{Map, Value};

use super::ctx::Ctx;

pub(super) fn jp(base: &str, key: &str) -> String {
    format!("{base}.{key}")
}

pub(super) fn as_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

pub(super) fn copy_string(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        match value.as_str() {
            Some(value) => {
                out.insert(key.to_owned(), Value::String(value.to_owned()));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

pub(super) fn copy_bool(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        match value.as_bool() {
            Some(value) => {
                out.insert(key.to_owned(), Value::Bool(value));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

pub(super) fn copy_object(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        match value.as_object() {
            Some(value) => {
                out.insert(key.to_owned(), Value::Object(value.clone()));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

pub(super) fn copy_bool_or_object(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        if value.is_boolean() || value.is_object() {
            out.insert(key.to_owned(), value.clone());
        } else {
            ctx.invalid(&jp("$", key));
        }
    }
}

pub(super) fn normalize_string_array(
    input: &Map<String, Value>,
    key: &str,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get(key) {
        match value.as_array() {
            Some(values) => {
                let mut result = Vec::new();
                for (index, item) in values.iter().enumerate() {
                    match item.as_str() {
                        Some(item) => result.push(Value::String(item.to_owned())),
                        None => ctx.invalid(&jp(&jp("$", key), &index.to_string())),
                    }
                }
                out.insert(key.to_owned(), Value::Array(result));
            }
            None => ctx.invalid(&jp("$", key)),
        }
    }
}

pub(super) fn normalize_update(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get("update") {
        match value.as_str() {
            Some("disable" | "notify" | "auto") => {
                out.insert("update".to_owned(), value.clone());
                return;
            }
            _ => ctx.invalid("$.update"),
        }
    }
    if let Some(value) = input.get("autoupdate") {
        let migrated = match value {
            Value::Bool(false) => Some("disable"),
            Value::Bool(true) => Some("auto"),
            Value::String(text) if text == "notify" => Some("notify"),
            _ => None,
        };
        match migrated {
            Some(value) => {
                out.insert("update".to_owned(), Value::String(value.to_owned()));
            }
            None => ctx.invalid("$.autoupdate"),
        }
    }
}

pub(super) fn normalize_share(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    if let Some(value) = input.get("share") {
        match value.as_str() {
            Some("manual" | "auto" | "disabled") => {
                out.insert("share".to_owned(), value.clone());
                return;
            }
            _ => ctx.invalid("$.share"),
        }
    }
    if let Some(value) = input.get("autoshare") {
        match value.as_bool() {
            Some(true) => {
                out.insert("share".to_owned(), Value::String("auto".to_owned()));
            }
            Some(false) => {}
            None => ctx.invalid("$.autoshare"),
        }
    }
}
