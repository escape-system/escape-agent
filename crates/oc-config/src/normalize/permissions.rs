//! Permission rule normalization and V1 tool/permission migration.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;

// ---------------------------------------------------------------------------
// Permissions and policies

pub(super) fn normalize_permissions(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut rules = Vec::new();
    if let Some(value) = input.get("tools") {
        rules.extend(migrate_tools(value, ctx, "$.tools"));
    }
    if let Some(value) = input.get("permission") {
        rules.extend(migrate_permission_v1(value, ctx, "$.permission"));
    }
    if let Some(value) = input.get("permissions") {
        rules.extend(normalize_rules(value, ctx, "$.permissions"));
    }
    if !rules.is_empty() || input.get("permissions").is_some_and(Value::is_array) {
        out.insert("permissions".to_owned(), Value::Array(rules));
    }
}

pub(super) fn migrate_tools(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let Some(entries) = value.as_object() else {
        ctx.invalid(base);
        return Vec::new();
    };
    let mut rules = Vec::new();
    for (action, enabled) in entries {
        let path = jp(base, action);
        match enabled.as_bool() {
            Some(enabled) => rules.push(rule(
                &normalize_action(action),
                "*",
                if enabled { "allow" } else { "deny" },
            )),
            None => ctx.invalid(&path),
        }
    }
    rules
}

pub(super) fn migrate_permission_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let mut rules = Vec::new();
    if let Some(effect) = value.as_str() {
        match crate::permissions::Effect::parse(effect) {
            Some(effect) => rules.push(rule("*", "*", effect.as_str())),
            None => ctx.invalid(base),
        }
        return rules;
    }
    let Some(entries) = value.as_object() else {
        ctx.invalid(base);
        return rules;
    };
    for (action, value) in entries {
        let path = jp(base, action);
        if let Some(effect) = value.as_str() {
            match crate::permissions::Effect::parse(effect) {
                Some(effect) => rules.push(rule(&normalize_action(action), "*", effect.as_str())),
                None => ctx.invalid(&path),
            }
            continue;
        }
        let Some(resources) = value.as_object() else {
            ctx.invalid(&path);
            continue;
        };
        for (resource, effect) in resources {
            match effect.as_str().and_then(crate::permissions::Effect::parse) {
                Some(effect) => {
                    rules.push(rule(&normalize_action(action), resource, effect.as_str()));
                }
                None => ctx.invalid(&jp(&path, resource)),
            }
        }
    }
    rules
}

pub(super) fn normalize_rules(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let Some(rules) = value.as_array() else {
        ctx.invalid(base);
        return Vec::new();
    };
    let mut result = Vec::new();
    for (index, item) in rules.iter().enumerate() {
        let path = jp(base, &index.to_string());
        let Some(object) = item.as_object() else {
            ctx.invalid(&path);
            continue;
        };
        let action = object.get("action").and_then(Value::as_str);
        let resource = object.get("resource").and_then(Value::as_str);
        let effect = object
            .get("effect")
            .and_then(Value::as_str)
            .and_then(crate::permissions::Effect::parse);
        match (action, resource, effect) {
            (Some(action), Some(resource), Some(effect)) => {
                result.push(rule(action, resource, effect.as_str()));
            }
            _ => ctx.invalid(&path),
        }
    }
    result
}

pub(super) fn rule(action: &str, resource: &str, effect: &str) -> Value {
    serde_json::json!({
        "action": action,
        "resource": resource,
        "effect": effect,
    })
}

pub(super) fn normalize_action(action: &str) -> String {
    match action {
        "write" | "patch" | "apply_patch" => "edit".to_owned(),
        "task" => "subagent".to_owned(),
        "bash" => "shell".to_owned(),
        other => other.to_owned(),
    }
}
