//! Skill list normalization.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;

pub(super) fn normalize_skills(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let Some(value) = input.get("skills") else {
        return;
    };
    if let Some(values) = value.as_array() {
        let mut result = Vec::new();
        for (index, item) in values.iter().enumerate() {
            match item.as_str() {
                Some(item) => result.push(Value::String(item.to_owned())),
                None => ctx.invalid(&jp("$.skills", &index.to_string())),
            }
        }
        out.insert("skills".to_owned(), Value::Array(result));
        return;
    }
    if let Some(object) = value.as_object() {
        let mut result = Vec::new();
        for key in ["paths", "urls"] {
            if let Some(values) = object.get(key) {
                match values.as_array() {
                    Some(values) => {
                        for (index, item) in values.iter().enumerate() {
                            match item.as_str() {
                                Some(item) => result.push(Value::String(item.to_owned())),
                                None => {
                                    ctx.invalid(&jp(&jp("$.skills", key), &index.to_string()));
                                }
                            }
                        }
                    }
                    None => ctx.invalid(&jp("$.skills", key)),
                }
            }
        }
        out.insert("skills".to_owned(), Value::Array(result));
        return;
    }
    ctx.invalid("$.skills");
}
