//! Policy normalization from provider lists and experimental policies.

use serde_json::{Map, Value};

use super::copy::{as_object, jp};
use super::ctx::Ctx;
use super::model::provider_id;

pub(super) fn normalize_policies(
    input: &Map<String, Value>,
    out: &mut Map<String, Value>,
    ctx: &mut Ctx,
) {
    let mut policies = Vec::new();
    if input.contains_key("enabled_providers")
        && let Some(values) =
            provider_list(input.get("enabled_providers"), "enabled_providers", ctx)
    {
        policies.push(policy("provider.use", "*", "deny"));
        for provider in values {
            policies.push(policy("provider.use", &provider_id(&provider), "allow"));
        }
    }
    if let Some(values) = provider_list(input.get("disabled_providers"), "disabled_providers", ctx)
    {
        for provider in values {
            policies.push(policy("provider.use", &provider_id(&provider), "deny"));
        }
    }
    if let Some(experimental) = as_object(input.get("experimental"))
        && let Some(value) = experimental.get("policies")
    {
        policies.extend(normalize_policy_list(value, ctx, "$.experimental.policies"));
    }
    if let Some(value) = input.get("policies") {
        policies.extend(normalize_policy_list(value, ctx, "$.policies"));
    }
    if !policies.is_empty()
        || input.get("policies").is_some_and(Value::is_array)
        || as_object(input.get("experimental"))
            .and_then(|experimental| experimental.get("policies"))
            .is_some_and(Value::is_array)
    {
        out.insert("policies".to_owned(), Value::Array(policies));
    }
}

pub(super) fn provider_list(
    value: Option<&Value>,
    key: &str,
    ctx: &mut Ctx,
) -> Option<Vec<String>> {
    let value = value?;
    let base = jp("$", key);
    let Some(values) = value.as_array() else {
        ctx.invalid(&base);
        return None;
    };
    let mut result = Vec::new();
    for (index, item) in values.iter().enumerate() {
        match item.as_str() {
            Some(item) => result.push(item.to_owned()),
            None => ctx.invalid(&jp(&base, &index.to_string())),
        }
    }
    Some(result)
}

pub(super) fn normalize_policy_list(value: &Value, ctx: &mut Ctx, base: &str) -> Vec<Value> {
    let Some(values) = value.as_array() else {
        ctx.invalid(base);
        return Vec::new();
    };
    let mut result = Vec::new();
    for (index, item) in values.iter().enumerate() {
        let path = jp(base, &index.to_string());
        let Some(object) = item.as_object() else {
            ctx.invalid(&path);
            continue;
        };
        let action = object.get("action").and_then(Value::as_str);
        let resource = object.get("resource").and_then(Value::as_str);
        let effect = object.get("effect").and_then(Value::as_str);
        if matches!(
            action,
            Some("provider.use" | "tool.use" | "integration.use")
        ) && resource.is_some()
            && matches!(effect, Some("allow" | "deny"))
        {
            result.push(policy(
                action.unwrap_or_default(),
                resource.unwrap_or_default(),
                effect.unwrap_or_default(),
            ));
        } else {
            ctx.invalid(&path);
        }
    }
    result
}

pub(super) fn policy(action: &str, resource: &str, effect: &str) -> Value {
    serde_json::json!({ "action": action, "resource": resource, "effect": effect })
}
