//! Configuration overlays applied on top of a parsed snapshot.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::LlmError;

use super::Catalog;
use super::convert::{compatibility_from_value, cost_from_config, protocol_for};
use super::reference::parse_reference;
use super::time::parse_release_date;
use super::types::{
    Activation, Compatibility, Cost, ModelInfo, PriceOverrideMap, Protocol, ProviderInfo, Variant,
};
use super::util::{merge_map, strings};
use super::variants::{reasoning_support_from_config, variants_for};

impl Catalog {
    /// Parse a snapshot and apply a merged `providers` configuration object.
    pub fn from_snapshot_with_config(json: &str, providers: &Value) -> Result<Self, LlmError> {
        let mut catalog = Self::from_snapshot(json)?;
        catalog.apply_config(providers)?;
        Ok(catalog)
    }

    /// Apply a merged configuration `providers` object. Unknown provider keys create
    /// a provider, exactly as a configured provider does upstream.
    pub fn apply_config(&mut self, providers: &Value) -> Result<(), LlmError> {
        let object = providers.as_object().ok_or_else(|| LlmError::Config {
            message: "`providers` must be a JSON object".to_string(),
        })?;
        for (provider_id, config) in object {
            self.apply_provider(provider_id, config)?;
        }
        self.reindex();
        Ok(())
    }

    fn apply_provider(&mut self, provider_id: &str, config: &Value) -> Result<(), LlmError> {
        let object = config.as_object().ok_or_else(|| LlmError::Config {
            message: format!("provider `{provider_id}` must be a JSON object"),
        })?;
        let existing = self.provider_pos(provider_id);
        let mut provider = match existing {
            Some(index) => self.providers[index].clone(),
            None => ProviderInfo {
                id: provider_id.to_string(),
                canonical: None,
                name: provider_id.to_string(),
                activation: Activation::Auto,
                package: String::new(),
                compatibility: None,
                settings: None,
                headers: None,
                body: None,
                env: Vec::new(),
            },
        };

        if let Some(canonical) = object.get("canonical").and_then(Value::as_str) {
            provider.canonical = Some(canonical.to_string());
        }
        if let Some(name) = object.get("name").and_then(Value::as_str)
            && !name.is_empty()
        {
            provider.name = name.to_string();
        }
        if let Some(package) = object.get("package").and_then(Value::as_str) {
            provider.package = package.to_string();
        }
        if let Some(env) = object.get("env").and_then(Value::as_array) {
            provider.env = env
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(activation) = object.get("activation").and_then(Value::as_str) {
            provider.activation = match activation {
                "enabled" => Activation::Enabled,
                "disabled" => Activation::Disabled,
                _ => Activation::Auto,
            };
        }
        if object.get("disabled").and_then(Value::as_bool) == Some(true) {
            provider.activation = Activation::Disabled;
        }
        if let Some(enabled) = object.get("enabled").and_then(Value::as_bool) {
            provider.activation = if enabled {
                Activation::Enabled
            } else {
                Activation::Disabled
            };
        }
        if let Some(compatibility) = object.get("compatibility").and_then(Value::as_object) {
            let overlay = compatibility_from_value(compatibility);
            provider
                .compatibility
                .get_or_insert_with(Compatibility::default)
                .merge(&overlay);
        }
        if let Some(settings) = object.get("settings").and_then(Value::as_object) {
            merge_map(
                Arc::make_mut(provider.settings.get_or_insert_with(Default::default)),
                settings,
            );
        }
        if let Some(headers) = object.get("headers").and_then(Value::as_object) {
            let merged = provider.headers.get_or_insert_with(BTreeMap::new);
            for (key, value) in headers {
                if let Some(value) = value.as_str() {
                    merged.insert(key.clone(), value.to_string());
                }
            }
        }
        if let Some(body) = object.get("body").and_then(Value::as_object) {
            merge_map(provider.body.get_or_insert_with(Map::new), body);
        }

        if let Some(index) = existing {
            self.providers[index] = provider;
        } else {
            let index = self.providers.len();
            self.providers.push(provider);
            self.insert_provider_sorted(index);
        }

        if let Some(models) = object.get("models").and_then(Value::as_object) {
            for (model_id, model_config) in models {
                self.apply_model(provider_id, model_id, model_config)?;
            }
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one overlay routine keeps provider/model config field precedence visible in one place"
    )]
    fn apply_model(
        &mut self,
        provider_id: &str,
        model_id: &str,
        config: &Value,
    ) -> Result<(), LlmError> {
        let object = config.as_object().ok_or_else(|| LlmError::Config {
            message: format!("model `{provider_id}/{model_id}` must be a JSON object"),
        })?;
        let existing = self.model_pos(provider_id, model_id);
        let mut model = match existing {
            Some(index) => self.models[index].clone(),
            None => ModelInfo::default_for(provider_id, model_id),
        };
        let provider_package = self
            .provider(provider_id)
            .map(|provider| provider.package.clone())
            .unwrap_or_default();

        if let Some(value) = object.get("modelID").and_then(Value::as_str) {
            model.model_id = value.to_string();
        }
        if let Some(value) = object.get("type").and_then(Value::as_str) {
            model.kind = Some(value.to_string());
        }
        if let Some(value) = object.get("family").and_then(Value::as_str) {
            model.family = Some(value.to_string());
        }
        if let Some(value) = object.get("name").and_then(Value::as_str)
            && !value.is_empty()
        {
            model.name = value.to_string();
        }
        if let Some(value) = object.get("releaseDate").and_then(Value::as_str) {
            model.time.released = parse_release_date(value);
        }
        if let Some(value) = object.get("package").and_then(Value::as_str) {
            model.package = Some(value.to_string());
        }
        let effective_package = model
            .package
            .clone()
            .unwrap_or_else(|| provider_package.clone());
        if object.contains_key("package") || model.protocol == Protocol::Unsupported {
            model.protocol = protocol_for(&effective_package);
        }
        if let Some(value) = object.get("compatibility").and_then(Value::as_object) {
            let overlay = compatibility_from_value(value);
            model
                .compatibility
                .get_or_insert_with(Default::default)
                .merge(&overlay);
        }
        if let Some(value) = object.get("settings").and_then(Value::as_object) {
            merge_map(
                Arc::make_mut(model.settings.get_or_insert_with(Default::default)),
                value,
            );
        }
        if let Some(value) = object.get("headers").and_then(Value::as_object) {
            let merged = model.headers.get_or_insert_with(Default::default);
            for (key, header) in value {
                if let Some(header) = header.as_str() {
                    merged.insert(key.clone(), header.to_string());
                }
            }
        }
        if let Some(value) = object.get("body").and_then(Value::as_object) {
            merge_map(model.body.get_or_insert_with(Default::default), value);
        }
        if let Some(value) = object.get("capabilities").and_then(Value::as_object) {
            if let Some(tools) = value.get("tools").and_then(Value::as_bool) {
                model.capabilities.tools = tools;
            }
            if let Some(input) = value.get("input").and_then(Value::as_array) {
                model.capabilities.input = strings(input);
            }
            if let Some(output) = value.get("output").and_then(Value::as_array) {
                model.capabilities.output = strings(output);
            }
            if let Some(temperature) = value.get("temperature").and_then(Value::as_bool) {
                model.capabilities.temperature = Some(temperature);
            }
            if let Some(reasoning) = value.get("reasoning") {
                let support = reasoning_support_from_config(reasoning);
                let variants = variants_for(&model, model.protocol, &support);
                model.variants = variants;
            }
        }
        if let Some(value) = object.get("limit").and_then(Value::as_object) {
            if let Some(context) = value.get("context").and_then(Value::as_u64) {
                model.limit.context = context;
            }
            if let Some(input) = value.get("input").and_then(Value::as_u64) {
                model.limit.input = Some(input);
            }
            if let Some(output) = value.get("output").and_then(Value::as_u64) {
                model.limit.output = output;
            }
        }
        if let Some(value) = object.get("cost") {
            model.cost = cost_from_config(value)?;
        } else if model.cost.is_empty()
            && !model.model_id.is_empty()
            && model.model_id != model_id
            && let Some(target) = self.model(provider_id, &model.model_id)
            && !target.cost.is_empty()
        {
            model.cost.clone_from(&target.cost);
        }
        if let Some(value) = object.get("variants").and_then(Value::as_array) {
            for entry in value {
                let Some(entry) = entry.as_object() else {
                    continue;
                };
                let Some(id) = entry.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let index = model.variants.iter().position(|variant| variant.id == id);
                let mut variant = index.map_or_else(
                    || Variant {
                        id: id.to_string(),
                        settings: None,
                        headers: None,
                        body: None,
                    },
                    |index| model.variants[index].clone(),
                );
                if let Some(settings) = entry.get("settings").and_then(Value::as_object) {
                    merge_map(
                        Arc::make_mut(variant.settings.get_or_insert_with(Default::default)),
                        settings,
                    );
                }
                if let Some(headers) = entry.get("headers").and_then(Value::as_object) {
                    let merged = variant.headers.get_or_insert_with(Default::default);
                    for (key, header) in headers {
                        if let Some(header) = header.as_str() {
                            merged.insert(key.clone(), header.to_string());
                        }
                    }
                }
                if let Some(body) = entry.get("body").and_then(Value::as_object) {
                    merge_map(variant.body.get_or_insert_with(Default::default), body);
                }
                match index {
                    Some(index) => model.variants[index] = variant,
                    None => model.variants.push(variant),
                }
            }
        }
        if let Some(disabled) = object.get("disabled").and_then(Value::as_bool) {
            model.enabled = !disabled;
        }
        if let Some(enabled) = object.get("enabled").and_then(Value::as_bool) {
            model.enabled = enabled;
        }

        if let Some(index) = existing {
            self.models[index] = model;
        } else {
            let index = self.models.len();
            self.models.push(model);
            self.insert_model_sorted(index);
        }
        Ok(())
    }

    /// Overlay user price overrides (`models.pricing`) onto the catalog.
    ///
    /// Keys are `provider/model[#variant]`; the variant is ignored because a
    /// price belongs to the model, not to a request variant. Each band is
    /// rewritten field by field, so an override that sets only `input` keeps
    /// the catalog `output` and cache prices. The hardcoded Antigravity zeros
    /// stay the default: nothing changes until an override names that model.
    /// A model with no band at all (`cost: []`) gets one so the override is not
    /// silently dropped. Unknown references and malformed keys are skipped.
    pub fn apply_price_overrides(&mut self, overrides: &PriceOverrideMap) {
        if overrides.is_empty() {
            return;
        }
        for (reference, price) in overrides {
            let Ok(parsed) = parse_reference(reference) else {
                continue;
            };
            let Some(index) = self.model_pos(&parsed.provider, &parsed.id) else {
                continue;
            };
            let model = &mut self.models[index];
            if model.cost.is_empty() {
                model.cost.push(Cost::default());
            }
            for band in &mut model.cost {
                *band = price.apply(band);
            }
        }
    }

    pub(super) fn reindex(&mut self) {
        self.sorted_providers.clear();
        self.sorted_providers.extend(0..self.providers.len());
        self.sorted_providers
            .sort_unstable_by(|&a, &b| self.providers[a].id.cmp(&self.providers[b].id));
        self.sorted_providers
            .dedup_by(|&mut a, &mut b| self.providers[a].id == self.providers[b].id);
        self.sorted_models.clear();
        self.sorted_models.extend(0..self.models.len());
        self.sorted_models.sort_unstable_by(|&a, &b| {
            (&self.models[a].provider_id, &self.models[a].id)
                .cmp(&(&self.models[b].provider_id, &self.models[b].id))
        });
        self.sorted_models.dedup_by(|&mut a, &mut b| {
            self.models[a].provider_id == self.models[b].provider_id
                && self.models[a].id == self.models[b].id
        });
        self.providers.shrink_to_fit();
        self.models.shrink_to_fit();
        self.sorted_providers.shrink_to_fit();
        self.sorted_models.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::catalog::types::PriceOverride;

    /// Same shape as the Antigravity built-ins: a model whose prices are all 0.
    const ZERO_PRICED: &str = r#"{
      "google": {
        "id": "google",
        "name": "Google",
        "env": ["GEMINI_API_KEY"],
        "models": {
          "gemini-3-pro": {
            "id": "gemini-3-pro",
            "type": "chat",
            "name": "Gemini 3 Pro",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "temperature": true},
            "limit": {"context": 1000000, "output": 65536},
            "cost": {"text": {"input": {"price": 0}, "output": {"price": 0}}},
            "api": {
              "generate-content": {
                "base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/google"}
              }
            }
          }
        }
      }
    }"#;

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn override_map() -> PriceOverrideMap {
        let mut map = PriceOverrideMap::new();
        map.insert(
            "google/gemini-3-pro".to_owned(),
            PriceOverride {
                input: Some(1.25),
                output: None,
                cache_read: Some(0.3),
                cache_write: None,
            },
        );
        map
    }

    #[test]
    fn override_replaces_zero_prices_and_keeps_unset_fields() {
        let mut catalog = Catalog::from_snapshot(ZERO_PRICED).expect("snapshot parses");
        catalog.apply_price_overrides(&override_map());
        let model = catalog
            .model("google", "gemini-3-pro")
            .expect("model present");
        assert_eq!(model.cost.len(), 1);
        assert_close(model.cost[0].input, 1.25);
        assert_close(model.cost[0].output, 0.0);
        assert_close(model.cost[0].cache.read, 0.3);
        assert_close(model.cost[0].cache.write, 0.0);
    }

    #[test]
    fn override_gives_a_band_to_a_model_that_had_no_cost() {
        let mut catalog = Catalog::default();
        catalog.upsert_provider_models(vec![ModelInfo::default_for("google", "gemini-3-pro")]);
        assert!(
            catalog
                .model("google", "gemini-3-pro")
                .expect("model")
                .cost
                .is_empty()
        );
        catalog.apply_price_overrides(&override_map());
        let model = catalog.model("google", "gemini-3-pro").expect("model");
        assert_eq!(model.cost.len(), 1);
        assert_close(model.cost[0].input, 1.25);
    }

    #[test]
    fn unknown_references_are_skipped() {
        let mut catalog = Catalog::from_snapshot(ZERO_PRICED).expect("snapshot parses");
        let mut overrides = PriceOverrideMap::new();
        for reference in ["missing/model", "not-a-reference"] {
            overrides.insert(
                reference.to_owned(),
                PriceOverride {
                    input: Some(9.0),
                    ..PriceOverride::default()
                },
            );
        }
        catalog.apply_price_overrides(&overrides);
        let model = catalog.model("google", "gemini-3-pro").expect("model");
        assert_close(model.cost[0].input, 0.0);
    }

    #[test]
    fn price_override_wire_shape_is_camel_case_and_omits_unset() {
        let parsed: PriceOverride =
            serde_json::from_value(json!({"input": 1.25, "cacheRead": 0.3}))
                .expect("parse override");
        assert_eq!(parsed.input, Some(1.25));
        assert_eq!(parsed.cache_read, Some(0.3));
        assert_eq!(parsed.cache_write, None);
        assert_eq!(
            serde_json::to_value(parsed).expect("serialize"),
            json!({"input": 1.25, "cacheRead": 0.3})
        );
    }

    #[test]
    fn aliased_config_model_inherits_catalog_cost_when_cost_omitted() {
        let mut catalog = Catalog::default();
        let mut target = ModelInfo::default_for("nous", "google/gemini-4-hydrogen");
        target.cost.push(Cost {
            input: 1.5,
            output: 7.5,
            ..Cost::default()
        });
        catalog.upsert_provider_models(vec![target]);

        catalog
            .apply_config(&json!({
                "nous": {
                    "models": {
                        "gemini-4-hydrogen": {
                            "modelID": "google/gemini-4-hydrogen",
                            "name": "Gemini 4 Hydrogen (via Nous)"
                        }
                    }
                }
            }))
            .expect("apply config");

        let alias = catalog
            .model("nous", "gemini-4-hydrogen")
            .expect("alias present");
        assert_eq!(alias.cost.len(), 1);
        assert_close(alias.cost[0].input, 1.5);
        assert_close(alias.cost[0].output, 7.5);
    }
}
