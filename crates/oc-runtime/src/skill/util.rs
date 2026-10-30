use oc_store::StoreError;
use oc_tools::{ToolContext, ToolError};
use oc_types::{EventEnvelope, EventLocation};
use serde_json::{Map, Value};

use crate::runtime::Runtime;

pub(super) fn publish(runtime: &Runtime, ctx: &ToolContext, kind: &str, data: Value) {
    let mut envelope = EventEnvelope::now(kind, data);
    envelope.location = Some(EventLocation {
        directory: ctx.directory.to_string_lossy().into_owned(),
    });
    runtime.inner.config.sink.publish(envelope);
}

pub(super) fn finding(severity: &str, code: &str, message: impl Into<String>) -> Value {
    let mut map = Map::new();
    map.insert("severity".to_owned(), Value::String(severity.to_owned()));
    map.insert("code".to_owned(), Value::String(code.to_owned()));
    map.insert("message".to_owned(), Value::String(message.into()));
    Value::Object(map)
}

pub(super) fn store_error(error: &StoreError) -> ToolError {
    ToolError::new(error.to_string())
}

pub(super) fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("`{key}` is required and must be non-empty")))
}

pub(super) fn optional_str(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(super) fn optional_bool(input: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::new(format!("`{key}` must be a boolean"))),
    }
}

pub(super) fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
