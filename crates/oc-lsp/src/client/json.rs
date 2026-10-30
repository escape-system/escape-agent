//! JSON helpers for the LSP wire.

use serde_json::Value;

pub(super) fn without_null_params(mut message: Value) -> Value {
    if message.get("params") == Some(&Value::Null)
        && let Some(object) = message.as_object_mut()
    {
        object.remove("params");
    }
    message
}

pub(super) fn configuration_value(settings: &Value, section: &str) -> Value {
    if section.is_empty() {
        return settings.clone();
    }
    let mut current = settings;
    for part in section.split('.') {
        match current.get(part) {
            Some(value) => current = value,
            None => return Value::Null,
        }
    }
    current.clone()
}
