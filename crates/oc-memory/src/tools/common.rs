//! Argument parsing helpers shared by the four memory tools.

use oc_tools::ToolError;
use serde_json::Value;

pub(super) fn string_array(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(value) => {
                        let value = value.trim();
                        if !value.is_empty() {
                            values.push(value.to_owned());
                        }
                    }
                    _ => {
                        return Err(ToolError::new(format!(
                            "Invalid arguments: `{key}` must be an array of strings"
                        )));
                    }
                }
            }
            Ok(values)
        }
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be an array of strings"
        ))),
    }
}

#[allow(clippy::cast_possible_truncation)] // clamped to 0.0..=1.0 first
pub(super) fn optional_confidence(input: &Value, key: &str) -> Result<Option<f32>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number.as_f64().ok_or_else(|| {
                ToolError::new(format!("Invalid arguments: `{key}` must be a number"))
            })?;
            Ok(Some(value.clamp(0.0, 1.0) as f32))
        }
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be a number"
        ))),
    }
}
