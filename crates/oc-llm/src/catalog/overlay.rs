//! Configuration overlays applied on top of a parsed snapshot.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::error::LlmError;

use super::Catalog;
use super::convert::{compatibility_from_value, cost_from_config, protocol_for};
use super::time::parse_release_date;
use super::types::{Activation, Compatibility, ModelInfo, Protocol, ProviderInfo, Variant};
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
        let existing = self.provider_index.get(provider_id).copied();
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
            merge_map(provider.settings.get_or_insert_with(Map::new), settings);
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
            self.provider_index
                .insert(provider.id.clone(), self.providers.len());
            self.providers.push(provider);
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
        let existing = self
            .model_index
            .get(&(provider_id.to_string(), model_id.to_string()))
            .copied();
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
                .get_or_insert_with(Compatibility::default)
                .merge(&overlay);
        }
        if let Some(value) = object.get("settings").and_then(Value::as_object) {
            merge_map(model.settings.get_or_insert_with(Map::new), value);
        }
        if let Some(value) = object.get("headers").and_then(Value::as_object) {
            let merged = model.headers.get_or_insert_with(BTreeMap::new);
            for (key, header) in value {
                if let Some(header) = header.as_str() {
                    merged.insert(key.clone(), header.to_string());
                }
            }
        }
        if let Some(value) = object.get("body").and_then(Value::as_object) {
            merge_map(model.body.get_or_insert_with(Map::new), value);
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
                    merge_map(variant.settings.get_or_insert_with(Map::new), settings);
                }
                if let Some(headers) = entry.get("headers").and_then(Value::as_object) {
                    let merged = variant.headers.get_or_insert_with(BTreeMap::new);
                    for (key, header) in headers {
                        if let Some(header) = header.as_str() {
                            merged.insert(key.clone(), header.to_string());
                        }
                    }
                }
                if let Some(body) = entry.get("body").and_then(Value::as_object) {
                    merge_map(variant.body.get_or_insert_with(Map::new), body);
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
            self.model_index.insert(
                (provider_id.to_string(), model_id.to_string()),
                self.models.len(),
            );
            self.models.push(model);
        }
        Ok(())
    }

    pub(super) fn reindex(&mut self) {
        self.provider_index.clear();
        for (index, provider) in self.providers.iter().enumerate() {
            self.provider_index.insert(provider.id.clone(), index);
        }
        self.model_index.clear();
        for (index, model) in self.models.iter().enumerate() {
            self.model_index
                .insert((model.provider_id.clone(), model.id.clone()), index);
        }
        self.providers.shrink_to_fit();
        self.models.shrink_to_fit();
        self.provider_index.shrink_to_fit();
        self.model_index.shrink_to_fit();
    }
}
