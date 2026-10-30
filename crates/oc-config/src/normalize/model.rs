//! Model selection parsing, migration, and formatting.

use serde_json::{Map, Value};

use super::copy::jp;
use super::ctx::Ctx;

// ---------------------------------------------------------------------------
// Model selections

pub(super) fn provider_id(provider: &str) -> String {
    match provider {
        "azure-cognitive-services" => "azure".to_owned(),
        "google-vertex-anthropic" => "google-vertex".to_owned(),
        other => other.to_owned(),
    }
}

pub(super) fn parse_model_ref(text: &str) -> Option<(String, String, Option<String>)> {
    let (reference, variant) = match text.split_once('#') {
        Some((reference, variant)) => {
            if variant.is_empty() || variant.contains('#') {
                return None;
            }
            (reference, Some(variant.to_owned()))
        }
        None => (text, None),
    };
    let (provider, model) = reference.split_once('/')?;
    if provider.is_empty()
        || provider.contains('/')
        || provider.contains('#')
        || model.is_empty()
        || model.contains('#')
    {
        return None;
    }
    Some((provider_id(provider), model.to_owned(), variant))
}

pub(super) fn model_value(provider: &str, model: &str, variant: Option<String>) -> Value {
    let mut map = Map::new();
    map.insert("providerID".to_owned(), Value::String(provider.to_owned()));
    map.insert("model".to_owned(), Value::String(model.to_owned()));
    if let Some(variant) = variant {
        map.insert("variant".to_owned(), Value::String(variant));
    }
    Value::Object(map)
}

pub(super) fn normalize_model(value: &Value, ctx: &mut Ctx, base: &str) -> Option<Value> {
    match value {
        Value::String(text) => {
            if let Some((provider, model, variant)) = parse_model_ref(text) {
                Some(model_value(&provider, &model, variant))
            } else {
                ctx.invalid(base);
                None
            }
        }
        Value::Object(object) => {
            let provider = object.get("providerID").and_then(Value::as_str);
            let model = object.get("model").and_then(Value::as_str);
            let (Some(provider), Some(model)) = (provider, model) else {
                ctx.invalid(base);
                return None;
            };
            if provider.is_empty()
                || provider.contains('/')
                || provider.contains('#')
                || model.is_empty()
                || model.contains('#')
            {
                ctx.invalid(base);
                return None;
            }
            let variant = match object.get("variant") {
                None => None,
                Some(Value::String(text)) if !text.is_empty() && !text.contains('#') => {
                    Some(text.clone())
                }
                Some(_) => {
                    ctx.invalid(&jp(base, "variant"));
                    return None;
                }
            };
            Some(model_value(&provider_id(provider), model, variant))
        }
        _ => {
            ctx.invalid(base);
            None
        }
    }
}

pub(super) fn migrate_model_v1(model: &str, variant: Option<&str>) -> Option<Value> {
    let (provider, model, embedded) = parse_model_ref(model)?;
    if embedded.is_some() {
        return None;
    }
    let variant = variant.and_then(|variant| {
        if variant.is_empty() || variant.contains('#') {
            None
        } else {
            Some(variant.to_owned())
        }
    });
    Some(model_value(&provider, &model, variant))
}

#[must_use]
pub(crate) fn model_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => parse_model_ref(text).map(|(provider, model, variant)| {
            variant.map_or_else(
                || format!("{provider}/{model}"),
                |variant| format!("{provider}/{model}#{variant}"),
            )
        }),
        Value::Object(object) => {
            let provider = object.get("providerID")?.as_str()?;
            let model = object.get("model")?.as_str()?;
            let variant = object.get("variant").and_then(Value::as_str);
            Some(variant.map_or_else(
                || format!("{provider}/{model}"),
                |variant| format!("{provider}/{model}#{variant}"),
            ))
        }
        _ => None,
    }
}

/// Convert a `provider/model[#variant]` reference or a
/// `{providerID, model, variant?}` object into the normalized model selection.
/// Invalid input yields `None`; callers without a diagnostic context (markdown
/// frontmatter) report it themselves.
pub(crate) fn model_selection(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => {
            let (provider, model, variant) = parse_model_ref(text)?;
            Some(model_value(&provider, &model, variant))
        }
        Value::Object(object) => {
            let provider = object.get("providerID")?.as_str()?;
            let model = object.get("model")?.as_str()?;
            if provider.is_empty()
                || provider.contains('/')
                || provider.contains('#')
                || model.is_empty()
                || model.contains('/')
                || model.contains('#')
            {
                return None;
            }
            let variant = match object.get("variant") {
                None => None,
                Some(Value::String(text)) if !text.is_empty() && !text.contains('#') => {
                    Some(text.clone())
                }
                Some(_) => return None,
            };
            Some(model_value(&provider_id(provider), model, variant))
        }
        _ => None,
    }
}
