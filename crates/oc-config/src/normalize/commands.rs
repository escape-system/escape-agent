//! Command normalization and V1 command migration.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;
use super::model::{migrate_model_v1, normalize_model};

pub(super) fn normalize_commands(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut commands = Map::new();
    if let Some(value) = input.get("command") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.command", name);
                    if let Some(command) = migrate_command_v1(item, ctx, &path) {
                        commands.insert(name.clone(), command);
                    }
                }
            }
            None => ctx.invalid("$.command"),
        }
    }
    if let Some(value) = input.get("commands") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.commands", name);
                    if let Some(command) = normalize_command_v2(item, ctx, &path) {
                        if commands.contains_key(name) {
                            ctx.conflict(&path);
                        }
                        commands.insert(name.clone(), command);
                    }
                }
            }
            None => ctx.invalid("$.commands"),
        }
    }
    if !commands.is_empty() || input.get("commands").is_some_and(Value::is_object) {
        out.insert("commands".to_owned(), Value::Object(commands));
    }
}

pub(super) fn migrate_command_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let template = object.get("template").and_then(Value::as_str).or_else(|| {
        ctx.invalid(&jp(base, "template"));
        None
    })?;
    let mut out = Map::new();
    out.insert("template".to_owned(), Value::String(template.to_owned()));
    if let Some(value) = object.get("description").and_then(Value::as_str) {
        out.insert("description".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(value) = object.get("agent").and_then(Value::as_str) {
        out.insert("agent".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(value) = object.get("model") {
        match value.as_str() {
            Some(model) => {
                let variant = object.get("variant").and_then(Value::as_str);
                if let Some(selection) = migrate_model_v1(model, variant) {
                    out.insert("model".to_owned(), selection);
                } else {
                    ctx.unsupported(
                        &jp(base, "model"),
                        "omitted unsupported legacy model reference",
                    );
                }
            }
            None => ctx.unsupported(
                &jp(base, "model"),
                "omitted unsupported legacy model reference",
            ),
        }
    }
    if let Some(value) = object.get("subtask").and_then(Value::as_bool) {
        out.insert("subagent".to_owned(), Value::Bool(value));
    }
    Some(Value::Object(out))
}

pub(super) fn normalize_command_v2(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let template = object.get("template").and_then(Value::as_str).or_else(|| {
        ctx.invalid(&jp(base, "template"));
        None
    })?;
    let mut out = Map::new();
    out.insert("template".to_owned(), Value::String(template.to_owned()));
    for key in ["description", "agent"] {
        if let Some(value) = object.get(key) {
            match value.as_str() {
                Some(value) => {
                    out.insert(key.to_owned(), Value::String(value.to_owned()));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
    if let Some(value) = object.get("model")
        && let Some(selection) = normalize_model(value, ctx, &jp(base, "model"))
    {
        out.insert("model".to_owned(), selection);
    }
    for key in ["subagent", "subtask"] {
        if let Some(value) = object.get(key) {
            match value.as_bool() {
                Some(value) => {
                    out.insert(key.to_owned(), Value::Bool(value));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
    Some(Value::Object(out))
}
