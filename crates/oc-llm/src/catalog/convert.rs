//! Source-to-catalog conversion: protocols, models, modalities, and costs.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::error::LlmError;

use super::cost::count;
use super::source::{Route, SourceCachePrices, SourceCost, SourceModel, SourcePrice, SourceWhen};
use super::time::parse_release_date;
use super::types::{
    Capabilities, Compatibility, Cost, CostCache, CostTier, CostTierKind, Limit, ModelInfo,
    ModelStatus, ModelTime, Protocol,
};

pub(super) fn protocol_for_route(api_name: &str, package: &str) -> Protocol {
    match api_name {
        "chat-completions" => Protocol::OpenAiChat,
        "messages" => Protocol::AnthropicMessages,
        "generate-content" => Protocol::Gemini,
        "responses" | "converse" => Protocol::Unsupported,
        _ => protocol_for(package),
    }
}

pub(super) fn protocol_for(package: &str) -> Protocol {
    let package = package.to_ascii_lowercase();
    if package.ends_with("/messages") || package.contains("anthropic") {
        Protocol::AnthropicMessages
    } else if package.contains("gemini")
        || package.ends_with("/google")
        || package.contains("/google/")
    {
        Protocol::Gemini
    } else if package.contains("openai") || package.ends_with("/chat") {
        Protocol::OpenAiChat
    } else {
        Protocol::Unsupported
    }
}

pub(super) fn default_base_url(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiChat => "https://api.openai.com/v1",
        Protocol::AnthropicMessages => "https://api.anthropic.com/v1",
        Protocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        Protocol::Unsupported => "",
    }
}

pub(super) fn model_info(
    provider_id: &str,
    provider_package: &str,
    provider_base_url: Option<&str>,
    source: &SourceModel,
    route: &Route,
    cost: Vec<Cost>,
) -> ModelInfo {
    let mut settings = Map::new();
    if route.base_url.as_deref() != provider_base_url
        && let Some(url) = &route.base_url
    {
        settings.insert("baseURL".to_string(), Value::String(url.clone()));
    }
    if route.package.contains("compatible") {
        settings.insert(
            "provider".to_string(),
            Value::String(provider_id.to_string()),
        );
    }
    let compatibility = route.compatibility.clone();
    let limit = source.limit.as_ref();
    ModelInfo {
        id: source.id.clone(),
        model_id: source.id.clone(),
        provider_id: provider_id.to_string(),
        canonical: None,
        kind: Some(source.kind.clone()),
        name: source.name.clone(),
        family: source.family.clone(),
        compatibility,
        package: (route.package != provider_package).then(|| route.package.clone()),
        settings: (!settings.is_empty()).then_some(settings),
        headers: None,
        body: None,
        capabilities: Capabilities {
            tools: source.capabilities.tools.supported,
            input: modalities(&source.modalities.input),
            output: modalities(&source.modalities.output),
            temperature: source.capabilities.temperature,
        },
        variants: Vec::new(),
        time: ModelTime {
            released: parse_release_date(&source.release_date),
        },
        cost,
        status: status_from(source.status.as_deref()),
        enabled: true,
        limit: Limit {
            context: limit.and_then(|limit| limit.context).map_or(0, count),
            input: limit.and_then(|limit| limit.input).map(count),
            output: limit.and_then(|limit| limit.output).map_or(0, count),
        },
        protocol: route.protocol,
    }
}

fn status_from(status: Option<&str>) -> ModelStatus {
    match status {
        Some("alpha") => ModelStatus::Alpha,
        Some("beta") => ModelStatus::Beta,
        Some("deprecated") => ModelStatus::Deprecated,
        _ => ModelStatus::Active,
    }
}

/// v2 lists exact MIME types; `OpenCode` capabilities use the broad kind.
fn modality(value: &str) -> String {
    if value == "application/pdf" {
        return "pdf".to_string();
    }
    let kind = value.split('/').next().unwrap_or(value);
    match kind {
        "text" | "image" | "audio" | "video" => kind.to_string(),
        _ => value.to_string(),
    }
}

fn modalities(values: &[String]) -> Vec<String> {
    let mut output = Vec::new();
    for value in values {
        let value = modality(value);
        if !output.contains(&value) {
            output.push(value);
        }
    }
    output
}

pub(super) fn cost_from_source(input: &SourceCost) -> Vec<Cost> {
    let Some(text) = &input.text else {
        return cost_from_v1(input);
    };
    let per_token = |price: &SourcePrice| price.unit.as_deref().is_none_or(|unit| unit == "token");
    let (Some(input_price), Some(output_price)) = (&text.input, &text.output) else {
        return vec![Cost::default()];
    };
    if !per_token(input_price) || !per_token(output_price) {
        return vec![Cost::default()];
    }
    let base = Cost {
        tier: None,
        input: input_price.price,
        output: output_price.price,
        cache: CostCache {
            read: input_price
                .cache
                .as_ref()
                .and_then(|cache| cache.read)
                .unwrap_or(0.0),
            write: cache_write(input_price.cache.as_ref()).unwrap_or(0.0),
        },
    };
    let mut tiers: BTreeMap<u64, Cost> = BTreeMap::new();
    for source in &input_price.overrides {
        if let Some(size) = context_size(&source.when) {
            let entry = tiers.entry(size).or_insert_with(|| Cost {
                tier: Some(CostTier {
                    kind: CostTierKind::Context,
                    size,
                }),
                ..base
            });
            if let Some(price) = source.price {
                entry.input = price;
            }
            if let Some(cache) = &source.cache {
                if let Some(read) = cache.read {
                    entry.cache.read = read;
                }
                if let Some(write) = cache_write(Some(cache)) {
                    entry.cache.write = write;
                }
            }
        }
    }
    for source in &output_price.overrides {
        if let Some(size) = context_size(&source.when) {
            let entry = tiers.entry(size).or_insert_with(|| Cost {
                tier: Some(CostTier {
                    kind: CostTierKind::Context,
                    size,
                }),
                ..base
            });
            if let Some(price) = source.price {
                entry.output = price;
            }
        }
    }
    let mut output = vec![base];
    output.extend(tiers.into_values());
    output
}

fn cost_from_v1(input: &SourceCost) -> Vec<Cost> {
    let base = Cost {
        tier: None,
        input: input.input.unwrap_or(0.0),
        output: input.output.unwrap_or(0.0),
        cache: CostCache {
            read: input.cache_read.unwrap_or(0.0),
            write: input.cache_write.unwrap_or(0.0),
        },
    };
    let mut output = vec![base];
    for tier in &input.tiers {
        output.push(Cost {
            tier: Some(CostTier {
                kind: CostTierKind::Context,
                size: tier.tier.size,
            }),
            input: tier.input.unwrap_or(base.input),
            output: tier.output.unwrap_or(base.output),
            cache: CostCache {
                read: tier.cache_read.unwrap_or(base.cache.read),
                write: tier.cache_write.unwrap_or(base.cache.write),
            },
        });
    }
    output
}

fn cache_write(cache: Option<&SourceCachePrices>) -> Option<f64> {
    let explicit = cache
        .and_then(|cache| cache.explicit.as_ref())
        .and_then(|explicit| explicit.write.as_ref())
        .and_then(|write| write.0);
    cache
        .and_then(|cache| cache.write.as_ref())
        .and_then(|write| write.0)
        .or(explicit)
}

fn context_size(when: &SourceWhen) -> Option<u64> {
    let context = when.context.as_ref()?;
    context.gt.or(context.gte).map(count)
}

pub(super) fn cost_from_config(value: &Value) -> Result<Vec<Cost>, LlmError> {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => {
            return Err(LlmError::Config {
                message: "`cost` must be an object or array".to_string(),
            });
        }
    };
    let mut costs = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object().ok_or_else(|| LlmError::Config {
            message: "each `cost` entry must be an object".to_string(),
        })?;
        let tier = object
            .get("tier")
            .and_then(Value::as_object)
            .and_then(|tier| {
                let size = tier.get("size").and_then(Value::as_u64)?;
                Some(CostTier {
                    kind: CostTierKind::Context,
                    size,
                })
            });
        costs.push(Cost {
            tier,
            input: object.get("input").and_then(Value::as_f64).unwrap_or(0.0),
            output: object.get("output").and_then(Value::as_f64).unwrap_or(0.0),
            cache: CostCache {
                read: object
                    .get("cache")
                    .and_then(Value::as_object)
                    .and_then(|cache| cache.get("read"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                write: object
                    .get("cache")
                    .and_then(Value::as_object)
                    .and_then(|cache| cache.get("write"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            },
        });
    }
    Ok(costs)
}

pub(super) fn compatibility_from_value(value: &Map<String, Value>) -> Compatibility {
    Compatibility {
        reasoning_field: value
            .get("reasoningField")
            .or_else(|| value.get("reasoning_field"))
            .and_then(Value::as_str)
            .map(str::to_string),
        max_tokens_field: value
            .get("maxTokensField")
            .or_else(|| value.get("max_tokens_field"))
            .and_then(Value::as_str)
            .map(str::to_string),
        media_input: value
            .get("mediaInput")
            .or_else(|| value.get("media_input"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}
