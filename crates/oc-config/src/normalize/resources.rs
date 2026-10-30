//! Reference, media, and plugin normalization.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;

pub(super) fn normalize_references(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut references = Map::new();
    for (key, base) in [("reference", "$.reference"), ("references", "$.references")] {
        let Some(value) = input.get(key) else {
            continue;
        };
        let Some(entries) = value.as_object() else {
            ctx.invalid(base);
            continue;
        };
        for (name, item) in entries {
            let path = jp(base, name);
            if item.is_string() || item.is_object() {
                if references.contains_key(name) {
                    ctx.conflict(&jp("$.references", name));
                }
                references.insert(name.clone(), item.clone());
            } else {
                ctx.invalid(&path);
            }
        }
    }
    if !references.is_empty() || input.get("references").is_some_and(Value::is_object) {
        out.insert("references".to_owned(), Value::Object(references));
    }
}

pub(super) fn normalize_media(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    for key in ["media", "attachment"] {
        let Some(value) = input.get(key) else {
            continue;
        };
        match value.as_object() {
            Some(object) => {
                out.insert("media".to_owned(), Value::Object(object.clone()));
                return;
            }
            None => {
                ctx.invalid(&jp("$", key));
            }
        }
    }
}

pub(super) fn normalize_plugins(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut plugins = Vec::new();
    if let Some(value) = input.get("plugin") {
        match value.as_array() {
            Some(entries) => {
                for (index, item) in entries.iter().enumerate() {
                    let path = jp("$.plugin", &index.to_string());
                    match item {
                        Value::String(package) => plugins.push(Value::String(package.clone())),
                        Value::Array(pair) => {
                            let package = pair.first().and_then(Value::as_str);
                            let options = pair.get(1);
                            match (package, options) {
                                (Some(package), Some(options)) => {
                                    let mut entry = Map::new();
                                    entry.insert(
                                        "package".to_owned(),
                                        Value::String(package.to_owned()),
                                    );
                                    entry.insert("options".to_owned(), options.clone());
                                    plugins.push(Value::Object(entry));
                                }
                                (Some(package), None) => {
                                    plugins.push(Value::String(package.to_owned()));
                                }
                                _ => ctx.invalid(&path),
                            }
                        }
                        _ => ctx.invalid(&path),
                    }
                }
            }
            None => ctx.invalid("$.plugin"),
        }
    }
    if let Some(value) = input.get("plugins") {
        match value.as_array() {
            Some(entries) => {
                for (index, item) in entries.iter().enumerate() {
                    let path = jp("$.plugins", &index.to_string());
                    match item {
                        Value::String(package) => plugins.push(Value::String(package.clone())),
                        Value::Object(object) => {
                            if object.get("package").is_some_and(Value::is_string) {
                                plugins.push(item.clone());
                            } else {
                                ctx.invalid(&path);
                            }
                        }
                        _ => ctx.invalid(&path),
                    }
                }
            }
            None => ctx.invalid("$.plugins"),
        }
    }
    if !plugins.is_empty()
        || input.get("plugin").is_some_and(Value::is_array)
        || input.get("plugins").is_some_and(Value::is_array)
    {
        out.insert("plugins".to_owned(), Value::Array(plugins));
    }
}
