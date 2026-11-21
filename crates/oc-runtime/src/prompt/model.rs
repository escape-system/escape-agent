use oc_types::ModelRef;
use serde_json::Value;

fn normalized_variant(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
        .map(str::to_owned)
}

/// Parse a model reference carried as a string or `{providerID,id,variant}`.
#[must_use]
pub fn model_ref(value: &Value) -> Option<ModelRef> {
    match value {
        Value::String(text) => parse_reference(text),
        Value::Object(object) => {
            let provider = object.get("providerID").and_then(Value::as_str)?;
            let id = object
                .get("id")
                .or_else(|| object.get("model"))
                .and_then(Value::as_str)?;
            Some(ModelRef {
                id: id.to_owned(),
                provider_id: provider.to_owned(),
                variant: normalized_variant(object.get("variant").and_then(Value::as_str)),
            })
        }
        _ => None,
    }
}

fn parse_reference(text: &str) -> Option<ModelRef> {
    let (reference, variant) = match text.split_once('#') {
        Some((reference, variant)) => (reference, normalized_variant(Some(variant))),
        None => (text, None),
    };
    let (provider, id) = reference.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some(ModelRef {
        id: id.to_owned(),
        provider_id: provider.to_owned(),
        variant,
    })
}
