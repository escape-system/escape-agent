//! models.dev v1 to v2 snapshot conversion.

use serde::Deserialize;
use serde_json::Value;

use super::Ordered;
use super::source::{
    FirstPrice, SourceApi, SourceBudget, SourceCachePrices, SourceCapabilities,
    SourceCompatibility, SourceContext, SourceCost, SourceModel, SourceOverride, SourcePrice,
    SourceProvider, SourceReasoning, SourceSdk, SourceTextCost, SourceTools, SourceWhen,
};

// ---------------------------------------------------------------------------
// v1 -> v2 conversion
// ---------------------------------------------------------------------------

pub(super) fn to_v2(providers: Vec<(String, SourceProvider)>) -> Vec<(String, SourceProvider)> {
    providers
        .into_iter()
        .map(|(key, provider)| {
            let npm = provider.npm.clone().unwrap_or_default();
            let provider_api = provider.api.clone().unwrap_or_default();
            let models = provider
                .models
                .0
                .into_iter()
                .map(|(id, model)| {
                    let model_npm = model
                        .provider
                        .as_ref()
                        .and_then(|provider| provider.npm.clone())
                        .unwrap_or_else(|| npm.clone());
                    let shape = model
                        .provider
                        .as_ref()
                        .and_then(|provider| provider.shape.clone());
                    let api_name = if shape.as_deref() == Some("responses")
                        || (shape.is_none() && responses_by_default(&model_npm))
                    {
                        "responses"
                    } else {
                        "chat-completions"
                    };
                    let base_url = model
                        .provider
                        .as_ref()
                        .and_then(|provider| provider.api.clone())
                        .unwrap_or_else(|| provider_api.clone());
                    let mut api = Ordered::<SourceApi>::default();
                    api.0.push((
                        api_name.to_string(),
                        SourceApi {
                            base_url: (!base_url.is_empty()).then_some(base_url),
                            sdk: Some(SourceSdk {
                                aisdk: Some(model_npm),
                                opencodeai: None,
                            }),
                            compatibility: interleaved_field(&model).map(|field| {
                                SourceCompatibility {
                                    reasoning_field: Some(field),
                                    ..SourceCompatibility::default()
                                }
                            }),
                        },
                    ));
                    let reasoning = reasoning_from_v1(&model);
                    let converted = SourceModel {
                        id: model.id.clone(),
                        kind: model.kind.clone(),
                        name: model.name,
                        family: model.family,
                        release_date: model.release_date,
                        status: model.status,
                        modalities: model.modalities,
                        capabilities: SourceCapabilities {
                            tools: SourceTools {
                                supported: model.tool_call.unwrap_or(false),
                            },
                            reasoning,
                            temperature: model.temperature,
                        },
                        limit: model.limit,
                        cost: model.cost.as_ref().map(cost_to_v2),
                        api,
                        tool_call: None,
                        reasoning: None,
                        reasoning_options: Vec::new(),
                        temperature: None,
                        interleaved: None,
                        provider: None,
                    };
                    (id, converted)
                })
                .collect();
            (
                key,
                SourceProvider {
                    id: provider.id,
                    name: provider.name,
                    env: provider.env,
                    npm: None,
                    api: None,
                    models: Ordered(models),
                },
            )
        })
        .collect()
}

fn interleaved_field(model: &SourceModel) -> Option<String> {
    match model.interleaved.as_ref()? {
        Value::String(field) => Some(field.clone()),
        Value::Object(object) => object
            .get("field")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn responses_by_default(npm: &str) -> bool {
    matches!(npm, "@ai-sdk/openai" | "@ai-sdk/azure" | "@ai-sdk/xai")
}

fn reasoning_from_v1(model: &SourceModel) -> SourceReasoning {
    if model.reasoning != Some(true) {
        return SourceReasoning::default();
    }
    let effort = model
        .reasoning_options
        .iter()
        .find(|option| option.kind == "effort")
        .and_then(|option| option.values.clone())
        .map(|values| {
            values
                .into_iter()
                .flatten()
                .filter(|value| value != "null")
                .collect()
        });
    let budget = model
        .reasoning_options
        .iter()
        .find(|option| option.kind == "budget_tokens")
        .map(|option| SourceBudget {
            min: option.min,
            max: option.max,
        });
    SourceReasoning {
        supported: true,
        toggle: model
            .reasoning_options
            .iter()
            .any(|option| option.kind == "toggle"),
        effort,
        budget,
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "context band sizes are compared as f64 and stay below exact-integer range"
)]
fn cost_to_v2(cost: &SourceCost) -> SourceCost {
    let mut input_overrides = Vec::new();
    let mut output_overrides = Vec::new();
    for tier in &cost.tiers {
        let when = SourceWhen {
            context: Some(SourceContext {
                gt: Some(tier.tier.size as f64),
                gte: None,
            }),
        };
        input_overrides.push(SourceOverride {
            when: when.clone(),
            price: tier.input,
            cache: Some(SourceCachePrices {
                read: tier.cache_read,
                write: tier.cache_write.map(FirstPrice::from),
                explicit: None,
            }),
        });
        output_overrides.push(SourceOverride {
            when,
            price: tier.output,
            cache: None,
        });
    }
    SourceCost {
        text: Some(SourceTextCost {
            input: Some(SourcePrice {
                unit: None,
                price: cost.input.unwrap_or(0.0),
                cache: Some(SourceCachePrices {
                    read: cost.cache_read,
                    write: cost.cache_write.map(FirstPrice::from),
                    explicit: None,
                }),
                overrides: input_overrides,
            }),
            output: Some(SourcePrice {
                unit: None,
                price: cost.output.unwrap_or(0.0),
                cache: None,
                overrides: output_overrides,
            }),
        }),
        ..SourceCost::default()
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct SourceV1Tier {
    pub(super) tier: SourceV1TierMeta,
    #[serde(default)]
    pub(super) input: Option<f64>,
    #[serde(default)]
    pub(super) output: Option<f64>,
    #[serde(default)]
    pub(super) cache_read: Option<f64>,
    #[serde(default)]
    pub(super) cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SourceV1TierMeta {
    pub(super) size: u64,
}

impl From<f64> for FirstPrice {
    fn from(value: f64) -> Self {
        FirstPrice(Some(value))
    }
}
