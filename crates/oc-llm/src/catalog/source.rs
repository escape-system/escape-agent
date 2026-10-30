//! models.dev v2 snapshot source schema and route selection.

use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::Ordered;
use super::convert::protocol_for_route;
use super::types::{Compatibility, Protocol};
use super::v1::SourceV1Tier;

// ---------------------------------------------------------------------------
// Snapshot source schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct SourceProvider {
    #[serde(default)]
    pub(super) id: String,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) env: Vec<String>,
    /// models.dev v1 only.
    #[serde(default)]
    pub(super) npm: Option<String>,
    /// models.dev v1 only.
    #[serde(default)]
    pub(super) api: Option<String>,
    #[serde(default)]
    pub(super) models: Ordered<SourceModel>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SourceModel {
    #[serde(default)]
    pub(super) id: String,
    #[serde(rename = "type", default = "chat_type")]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) family: Option<String>,
    #[serde(default)]
    pub(super) release_date: String,
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) modalities: SourceModalities,
    #[serde(default)]
    pub(super) capabilities: SourceCapabilities,
    #[serde(default)]
    pub(super) limit: Option<SourceLimit>,
    #[serde(default)]
    pub(super) cost: Option<SourceCost>,
    #[serde(default)]
    pub(super) api: Ordered<SourceApi>,
    // models.dev v1 fields.
    #[serde(default)]
    pub(super) tool_call: Option<bool>,
    #[serde(default)]
    pub(super) reasoning: Option<bool>,
    #[serde(default)]
    pub(super) reasoning_options: Vec<SourceReasoningOption>,
    #[serde(default)]
    pub(super) temperature: Option<bool>,
    #[serde(default)]
    pub(super) interleaved: Option<Value>,
    #[serde(default)]
    pub(super) provider: Option<SourceModelProvider>,
}

fn chat_type() -> String {
    "chat".to_string()
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceModelProvider {
    #[serde(default)]
    pub(super) npm: Option<String>,
    #[serde(default)]
    pub(super) api: Option<String>,
    #[serde(default)]
    pub(super) shape: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct SourceModalities {
    #[serde(default = "text_modalities")]
    pub(super) input: Vec<String>,
    #[serde(default = "text_modalities")]
    pub(super) output: Vec<String>,
}

impl Default for SourceModalities {
    fn default() -> Self {
        Self {
            input: text_modalities(),
            output: text_modalities(),
        }
    }
}

fn text_modalities() -> Vec<String> {
    vec!["text".to_string()]
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceCapabilities {
    #[serde(default)]
    pub(super) tools: SourceTools,
    #[serde(default)]
    pub(super) reasoning: SourceReasoning,
    #[serde(default)]
    pub(super) temperature: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceTools {
    #[serde(default)]
    pub(super) supported: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct SourceReasoning {
    #[serde(default)]
    pub(super) supported: bool,
    #[serde(default)]
    pub(super) toggle: bool,
    #[serde(default)]
    pub(super) effort: Option<Vec<String>>,
    #[serde(default)]
    pub(super) budget: Option<SourceBudget>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct SourceBudget {
    #[serde(default)]
    pub(super) min: Option<i64>,
    #[serde(default)]
    pub(super) max: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceReasoningOption {
    #[serde(rename = "type", default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) values: Option<Vec<Option<String>>>,
    #[serde(default)]
    pub(super) min: Option<i64>,
    #[serde(default)]
    pub(super) max: Option<i64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct SourceLimit {
    #[serde(default)]
    pub(super) context: Option<f64>,
    #[serde(default)]
    pub(super) input: Option<f64>,
    #[serde(default)]
    pub(super) output: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceApi {
    #[serde(default)]
    pub(super) base_url: Option<String>,
    #[serde(default)]
    pub(super) sdk: Option<SourceSdk>,
    #[serde(default)]
    pub(super) compatibility: Option<SourceCompatibility>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceSdk {
    #[serde(default)]
    pub(super) aisdk: Option<String>,
    #[serde(default)]
    pub(super) opencodeai: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceCompatibility {
    #[serde(default)]
    pub(super) reasoning_field: Option<String>,
    #[serde(default)]
    pub(super) max_tokens_field: Option<String>,
    #[serde(default)]
    pub(super) media_input: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceCost {
    #[serde(default)]
    pub(super) text: Option<SourceTextCost>,
    // models.dev v1 cost fields share the same object.
    #[serde(default)]
    pub(super) input: Option<f64>,
    #[serde(default)]
    pub(super) output: Option<f64>,
    #[serde(default)]
    pub(super) cache_read: Option<f64>,
    #[serde(default)]
    pub(super) cache_write: Option<f64>,
    #[serde(default)]
    pub(super) tiers: Vec<SourceV1Tier>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceTextCost {
    #[serde(default)]
    pub(super) input: Option<SourcePrice>,
    #[serde(default)]
    pub(super) output: Option<SourcePrice>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourcePrice {
    #[serde(default)]
    pub(super) unit: Option<String>,
    #[serde(default)]
    pub(super) price: f64,
    #[serde(default)]
    pub(super) cache: Option<SourceCachePrices>,
    #[serde(default)]
    pub(super) overrides: Vec<SourceOverride>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceCachePrices {
    #[serde(default)]
    pub(super) read: Option<f64>,
    #[serde(default)]
    pub(super) write: Option<FirstPrice>,
    #[serde(default)]
    pub(super) explicit: Option<SourceExplicitCache>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceExplicitCache {
    #[serde(default)]
    pub(super) write: Option<FirstPrice>,
}

/// A cache-write price is either a number or a TTL map; upstream keeps the first entry.
#[derive(Debug, Default)]
pub(super) struct FirstPrice(pub(super) Option<f64>);

#[allow(
    clippy::cast_precision_loss,
    reason = "cache price values are small token prices where f64 is exact enough"
)]
impl<'de> Deserialize<'de> for FirstPrice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PriceVisitor;

        impl<'de> Visitor<'de> for PriceVisitor {
            type Value = FirstPrice;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a number or a TTL map of numbers")
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(FirstPrice(Some(value)))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(FirstPrice(Some(value as f64)))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(FirstPrice(Some(value as f64)))
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut first = None;
                while let Some((_key, value)) = map.next_entry::<String, f64>()? {
                    if first.is_none() {
                        first = Some(value);
                    }
                }
                Ok(FirstPrice(first))
            }
        }

        deserializer.deserialize_any(PriceVisitor)
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct SourceOverride {
    #[serde(default)]
    pub(super) when: SourceWhen,
    #[serde(default)]
    pub(super) price: Option<f64>,
    #[serde(default)]
    pub(super) cache: Option<SourceCachePrices>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct SourceWhen {
    #[serde(default)]
    pub(super) context: Option<SourceContext>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(super) struct SourceContext {
    #[serde(default)]
    pub(super) gt: Option<f64>,
    #[serde(default)]
    pub(super) gte: Option<f64>,
}

#[derive(Debug, Clone)]
pub(super) struct Route {
    pub(super) package: String,
    pub(super) base_url: Option<String>,
    pub(super) compatibility: Option<Compatibility>,
    pub(super) protocol: Protocol,
}

pub(super) fn select_route(model: &SourceModel) -> Option<Route> {
    let mut candidates: Vec<(String, &SourceApi, String)> = model
        .api
        .0
        .iter()
        .filter_map(|(protocol, api)| {
            let package = api
                .sdk
                .as_ref()
                .and_then(|sdk| sdk.opencodeai.clone().or_else(|| sdk.aisdk.clone()))?;
            Some((protocol.clone(), api, package))
        })
        .collect();

    if model.kind != "chat" {
        candidates.retain(|(_, _, package)| package.ends_with(&format!("/{}s", model.kind)));
    }
    if candidates.is_empty() {
        return None;
    }

    let chosen = candidates
        .iter()
        .find(|(protocol, _, package)| {
            matches!(
                protocol_for_route(protocol, package),
                Protocol::OpenAiChat | Protocol::AnthropicMessages | Protocol::Gemini
            )
        })
        .or_else(|| candidates.first())?;

    let (protocol_name, api, package) = chosen;
    let protocol = protocol_for_route(protocol_name, package);
    let compatibility = api
        .compatibility
        .as_ref()
        .map(|compatibility| Compatibility {
            reasoning_field: compatibility.reasoning_field.clone(),
            max_tokens_field: compatibility.max_tokens_field.clone(),
            media_input: compatibility.media_input.clone(),
        });
    Some(Route {
        package: package.clone(),
        base_url: api.base_url.clone().filter(|url| !url.is_empty()),
        compatibility,
        protocol,
    })
}
