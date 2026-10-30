//! Agent normalization and V1 agent migration.

use serde_json::{Map, Value};

use super::copy::{as_object, jp};
use super::ctx::Ctx;
use super::model::{migrate_model_v1, model_value, normalize_model, parse_model_ref};
use super::permissions::{migrate_permission_v1, migrate_tools, normalize_rules};

// ---------------------------------------------------------------------------
// Agents and commands

pub(super) fn normalize_agents(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut agents = Map::new();
    if let Some(value) = input.get("agent") {
        match value.as_object() {
            Some(entries) => {
                for (id, item) in entries {
                    let path = jp("$.agent", id);
                    if let Some(agent) = migrate_agent_v1(item, None, ctx, &path) {
                        agents.insert(id.clone(), agent);
                    }
                }
            }
            None => ctx.invalid("$.agent"),
        }
    }
    if let Some(value) = input.get("mode") {
        match value.as_object() {
            Some(entries) => {
                for (id, item) in entries {
                    let path = jp("$.mode", id);
                    if let Some(agent) = migrate_agent_v1(item, Some("primary"), ctx, &path) {
                        if agents.contains_key(id) {
                            ctx.conflict(&jp("$.agents", id));
                        }
                        agents.insert(id.clone(), agent);
                    }
                }
            }
            None => ctx.invalid("$.mode"),
        }
    }
    if let Some(value) = input.get("small_model") {
        match value.as_str().and_then(parse_model_ref) {
            Some((provider, model, variant)) => {
                let mut title = agents
                    .remove("title")
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                if !title.contains_key("model") {
                    title.insert("model".to_owned(), model_value(&provider, &model, variant));
                }
                agents.insert("title".to_owned(), Value::Object(title));
            }
            None => ctx.unsupported(
                "$.small_model",
                "omitted unsupported legacy model reference",
            ),
        }
    }
    if let Some(value) = input.get("agents") {
        match value.as_object() {
            Some(entries) => {
                for (id, item) in entries {
                    let path = jp("$.agents", id);
                    if let Some(agent) = normalize_agent_v2(item, ctx, &path) {
                        if agents.contains_key(id) {
                            ctx.conflict(&path);
                        }
                        agents.insert(id.clone(), agent);
                    }
                }
            }
            None => ctx.invalid("$.agents"),
        }
    }
    if !agents.is_empty() || input.get("agents").is_some_and(Value::is_object) {
        out.insert("agents".to_owned(), Value::Object(agents));
    }
}

pub(super) fn migrate_agent_v1(
    value: &Value,
    forced_mode: Option<&str>,
    ctx: &mut Ctx,
    base: &str,
) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let mut out = Map::new();

    if let Some(model) = object.get("model") {
        match model.as_str() {
            Some(model) => {
                let variant = object.get("variant").and_then(Value::as_str);
                match migrate_model_v1(model, variant) {
                    Some(selection) => {
                        out.insert("model".to_owned(), selection);
                    }
                    None => ctx.unsupported(
                        &jp(base, "model"),
                        "omitted unsupported legacy model reference",
                    ),
                }
            }
            None => ctx.unsupported(
                &jp(base, "model"),
                "omitted unsupported legacy model reference",
            ),
        }
    }

    let mut body = Map::new();
    if let Some(options) = as_object(object.get("options")) {
        for (key, value) in options {
            body.insert(key.clone(), value.clone());
        }
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = object.get(key) {
            if value.is_number() {
                body.insert(key.to_owned(), value.clone());
            } else {
                ctx.invalid(&jp(base, key));
            }
        }
    }
    if !body.is_empty() {
        out.insert(
            "request".to_owned(),
            serde_json::json!({ "body": Value::Object(body) }),
        );
    }

    if let Some(value) = object.get("prompt").and_then(Value::as_str) {
        out.insert("system".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(value) = object.get("description").and_then(Value::as_str) {
        out.insert("description".to_owned(), Value::String(value.to_owned()));
    }
    if let Some(mode) = forced_mode.or_else(|| object.get("mode").and_then(Value::as_str)) {
        if matches!(mode, "subagent" | "primary" | "all") {
            out.insert("mode".to_owned(), Value::String(mode.to_owned()));
        } else {
            ctx.invalid(&jp(base, "mode"));
        }
    }
    if let Some(value) = object.get("hidden").and_then(Value::as_bool) {
        out.insert("hidden".to_owned(), Value::Bool(value));
    }
    if let Some(value) = object.get("color") {
        match value.as_str() {
            Some(color) if is_hex_color(color) => {
                out.insert("color".to_owned(), Value::String(color.to_owned()));
            }
            Some(_) => {
                // V1 theme names fall back to a neutral color upstream.
                out.insert("color".to_owned(), Value::String("#aaaaaa".to_owned()));
            }
            None => ctx.invalid(&jp(base, "color")),
        }
    }
    let steps = object
        .get("steps")
        .or_else(|| object.get("maxSteps"))
        .and_then(Value::as_u64)
        .filter(|steps| *steps > 0);
    if let Some(steps) = steps {
        out.insert("steps".to_owned(), Value::from(steps));
    }
    if let Some(value) = object.get("disable").and_then(Value::as_bool) {
        out.insert("disabled".to_owned(), Value::Bool(value));
    }

    let mut permissions = Vec::new();
    if let Some(value) = object.get("tools") {
        permissions.extend(migrate_tools(value, ctx, &jp(base, "tools")));
    }
    if let Some(value) = object.get("permission") {
        permissions.extend(migrate_permission_v1(value, ctx, &jp(base, "permission")));
    }
    if !permissions.is_empty() {
        out.insert("permissions".to_owned(), Value::Array(permissions));
    }

    Some(Value::Object(out))
}

#[allow(clippy::too_many_lines)] // Field-by-field mapping keeps the accepted agent keys auditable in one place.
pub(super) fn normalize_agent_v2(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let object = value.as_object().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    let mut out = Map::new();
    for (key, value) in object {
        let known = matches!(
            key.as_str(),
            "model"
                | "request"
                | "system"
                | "description"
                | "mode"
                | "hidden"
                | "color"
                | "steps"
                | "disabled"
                | "permissions"
        );
        if !known {
            out.insert(key.clone(), value.clone());
        }
    }
    if let Some(value) = object.get("model")
        && let Some(selection) = normalize_model(value, ctx, &jp(base, "model"))
    {
        out.insert("model".to_owned(), selection);
    }
    if let Some(value) = object.get("request") {
        match value.as_object() {
            Some(request) => {
                let mut normalized = Map::new();
                if let Some(headers) = request.get("headers") {
                    match headers.as_object() {
                        Some(headers) if headers.values().all(Value::is_string) => {
                            normalized.insert("headers".to_owned(), Value::Object(headers.clone()));
                        }
                        _ => ctx.invalid(&jp(&jp(base, "request"), "headers")),
                    }
                }
                if let Some(body) = request.get("body") {
                    match body.as_object() {
                        Some(body) => {
                            normalized.insert("body".to_owned(), Value::Object(body.clone()));
                        }
                        None => ctx.invalid(&jp(&jp(base, "request"), "body")),
                    }
                }
                out.insert("request".to_owned(), Value::Object(normalized));
            }
            None => ctx.invalid(&jp(base, "request")),
        }
    }
    for key in ["system", "description"] {
        if let Some(value) = object.get(key) {
            match value.as_str() {
                Some(value) => {
                    out.insert(key.to_owned(), Value::String(value.to_owned()));
                }
                None => ctx.invalid(&jp(base, key)),
            }
        }
    }
    if let Some(value) = object.get("mode") {
        match value.as_str() {
            Some(mode @ ("subagent" | "primary" | "all")) => {
                out.insert("mode".to_owned(), Value::String(mode.to_owned()));
            }
            _ => ctx.invalid(&jp(base, "mode")),
        }
    }
    if let Some(value) = object.get("hidden") {
        match value.as_bool() {
            Some(value) => {
                out.insert("hidden".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid(&jp(base, "hidden")),
        }
    }
    if let Some(value) = object.get("color") {
        match value.as_str() {
            Some(color) if is_hex_color(color) => {
                out.insert("color".to_owned(), Value::String(color.to_owned()));
            }
            _ => ctx.invalid(&jp(base, "color")),
        }
    }
    if let Some(value) = object.get("steps") {
        match value.as_u64().filter(|steps| *steps > 0) {
            Some(steps) => {
                out.insert("steps".to_owned(), Value::from(steps));
            }
            None => ctx.invalid(&jp(base, "steps")),
        }
    }
    if let Some(value) = object.get("disabled") {
        match value.as_bool() {
            Some(value) => {
                out.insert("disabled".to_owned(), Value::Bool(value));
            }
            None => ctx.invalid(&jp(base, "disabled")),
        }
    }
    if let Some(value) = object.get("permissions") {
        let rules = normalize_rules(value, ctx, &jp(base, "permissions"));
        out.insert("permissions".to_owned(), Value::Array(rules));
    }
    Some(Value::Object(out))
}

pub(super) fn is_hex_color(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value[1..].chars().all(|c| c.is_ascii_hexdigit())
}
