//! Provider normalization and V1 provider migration.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;
use super::model::provider_id;

pub(super) fn normalize_providers(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut providers = Map::new();
    if let Some(value) = input.get("provider") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.provider", name);
                    if let Some(provider) = normalize_provider_v1(item, ctx, &path) {
                        providers.insert(provider_id(name), provider);
                    }
                }
            }
            None => ctx.invalid("$.provider"),
        }
    }
    if let Some(value) = input.get("providers") {
        match value.as_object() {
            Some(entries) => {
                for (name, item) in entries {
                    let path = jp("$.providers", name);
                    match item.as_object() {
                        Some(_) => {
                            if providers.contains_key(name) {
                                ctx.conflict(&path);
                            }
                            providers.insert(name.clone(), item.clone());
                        }
                        None => ctx.invalid(&path),
                    }
                }
            }
            None => ctx.invalid("$.providers"),
        }
    }
    if !providers.is_empty() || input.get("providers").is_some_and(Value::is_object) {
        out.insert("providers".to_owned(), Value::Object(providers));
    }
}

pub(super) fn normalize_provider_v1(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    let mut object = value.as_object().cloned().or_else(|| {
        ctx.invalid(base);
        None
    })?;
    // V1 `npm` is the package specifier; V2 calls it `package`.
    if let Some(value) = object.remove("npm") {
        match value.as_str() {
            Some(package) => {
                object
                    .entry("package".to_owned())
                    .or_insert_with(|| Value::String(package.to_owned()));
            }
            None => ctx.invalid(&jp(base, "npm")),
        }
    }
    // V1 `options` carried request overlays; split them into headers/body/settings.
    if let Some(options) = object.remove("options") {
        let Some(options) = options.as_object() else {
            ctx.invalid(&jp(base, "options"));
            return Some(Value::Object(object));
        };
        if let Some(headers) = options.get("headers") {
            if headers
                .as_object()
                .is_some_and(|headers| headers.values().all(Value::is_string))
            {
                object
                    .entry("headers".to_owned())
                    .or_insert_with(|| headers.clone());
            } else {
                ctx.invalid(&jp(&jp(base, "options"), "headers"));
            }
        }
        if let Some(body) = options.get("body") {
            if body.is_object() {
                object
                    .entry("body".to_owned())
                    .or_insert_with(|| body.clone());
            } else {
                ctx.invalid(&jp(&jp(base, "options"), "body"));
            }
        }
        let mut settings = Map::new();
        for (key, value) in options {
            if key != "headers" && key != "body" {
                settings.insert(key.clone(), value.clone());
            }
        }
        if !settings.is_empty() {
            object
                .entry("settings".to_owned())
                .or_insert_with(|| Value::Object(settings));
        }
    }
    Some(Value::Object(object))
}
