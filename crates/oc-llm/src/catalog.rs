//! Bundled models.dev catalog, model resolution, and configuration overlays.
//!
//! The snapshot is embedded at build time, parsed once on first use, and never
//! performs network or file I/O. Configuration is supplied as an already-merged
//! `providers` JSON object.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::LazyLock;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::LlmError;
use crate::event::Usage;

/// Bundled models.dev v2 snapshot (7.9 MB).
pub const EMBEDDED_SNAPSHOT: &str = include_str!("../data/models-snapshot.json");

static EMBEDDED: LazyLock<Result<Catalog, String>> =
    LazyLock::new(|| Catalog::from_snapshot(EMBEDDED_SNAPSHOT).map_err(|error| error.to_string()));

/// An ordered JSON object. Models.dev uses object insertion order to pick a
/// model's default API, so the catalog cannot rely on `serde_json::Map` order.
#[derive(Debug)]
pub struct Ordered<T>(pub Vec<(String, T)>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Ordered<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for OrderedVisitor<T> {
            type Value = Ordered<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0));
                while let Some((key, value)) = map.next_entry::<String, T>()? {
                    entries.push((key, value));
                }
                Ok(Ordered(entries))
            }
        }

        deserializer.deserialize_map(OrderedVisitor(std::marker::PhantomData))
    }
}

impl<T> Default for Ordered<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

/// Provider activation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Activation {
    #[default]
    Auto,
    Enabled,
    Disabled,
}

/// Model lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    Alpha,
    Beta,
    Deprecated,
    #[default]
    Active,
}

/// Provider/model-wide compatibility switches read by protocol adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Compatibility {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_input: Option<String>,
}

impl Compatibility {
    fn merge(&mut self, overlay: &Compatibility) {
        if overlay.reasoning_field.is_some() {
            self.reasoning_field.clone_from(&overlay.reasoning_field);
        }
        if overlay.max_tokens_field.is_some() {
            self.max_tokens_field.clone_from(&overlay.max_tokens_field);
        }
        if overlay.media_input.is_some() {
            self.media_input.clone_from(&overlay.media_input);
        }
    }
}

/// Broad modality capabilities used by the UI and runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub input: Vec<String>,
    pub output: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
}

impl Capabilities {
    #[must_use]
    pub fn default_capabilities() -> Self {
        Self {
            tools: true,
            input: vec!["text".to_string(), "image".to_string()],
            output: vec!["text".to_string()],
            temperature: None,
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::default_capabilities()
    }
}

/// Context and output token limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limit {
    pub context: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    pub output: u64,
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            context: 200_000,
            input: None,
            output: 32_000,
        }
    }
}

/// A context-size price band. Prices are USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<CostTier>,
    pub input: f64,
    pub output: f64,
    pub cache: CostCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostTier {
    #[serde(rename = "type")]
    pub kind: CostTierKind,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostTierKind {
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CostCache {
    pub read: f64,
    pub write: f64,
}

/// Request overrides attached to a variant.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Map<String, Value>>,
}

/// One catalog model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<Compatibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Map<String, Value>>,
    pub capabilities: Capabilities,
    pub variants: Vec<Variant>,
    pub time: ModelTime,
    pub cost: Vec<Cost>,
    pub status: ModelStatus,
    pub enabled: bool,
    pub limit: Limit,
    /// Selected wire protocol. Persisted across config overlays, not serialized.
    #[serde(skip)]
    pub protocol: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelTime {
    pub released: i64,
}

impl ModelInfo {
    /// Defaults used when configuration introduces a model that is not in the snapshot.
    #[must_use]
    pub fn default_for(provider_id: &str, id: &str) -> Self {
        Self {
            id: id.to_string(),
            model_id: id.to_string(),
            provider_id: provider_id.to_string(),
            canonical: None,
            kind: Some("chat".to_string()),
            name: id.to_string(),
            family: None,
            compatibility: None,
            package: None,
            settings: None,
            headers: None,
            body: None,
            capabilities: Capabilities::default(),
            variants: Vec::new(),
            time: ModelTime::default(),
            cost: Vec::new(),
            status: ModelStatus::Active,
            enabled: true,
            limit: Limit::default(),
            protocol: Protocol::Unsupported,
        }
    }
}

/// One catalog provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    pub name: String,
    pub activation: Activation,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<Compatibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Map<String, Value>>,
    /// Environment variables that can carry this provider's credential. Extension
    /// over `Provider.Info`, needed by the transport to find a key without shelling out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
}

/// Wire protocol spoken by a resolved model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenAiChat,
    AnthropicMessages,
    Gemini,
    #[default]
    Unsupported,
}

impl Protocol {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "openai-chat",
            Protocol::AnthropicMessages => "anthropic-messages",
            Protocol::Gemini => "gemini",
            Protocol::Unsupported => "unsupported",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A model plus its provider, variant, and transport route.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModel {
    pub model: ModelInfo,
    pub provider: ProviderInfo,
    pub variant: Option<Variant>,
    pub protocol: Protocol,
    pub base_url: String,
    pub reference: String,
}

impl ResolvedModel {
    /// Deep-merged settings: provider, then model, then variant.
    #[must_use]
    pub fn settings(&self) -> Map<String, Value> {
        let mut merged = self.provider.settings.clone().unwrap_or_default();
        if let Some(model) = &self.model.settings {
            merge_map(&mut merged, model);
        }
        if let Some(variant) = &self.variant
            && let Some(settings) = &variant.settings
        {
            merge_map(&mut merged, settings);
        }
        merged
    }

    /// Request body overlay: provider, then model, then variant.
    #[must_use]
    pub fn body(&self) -> Map<String, Value> {
        let mut merged = self.provider.body.clone().unwrap_or_default();
        if let Some(body) = &self.model.body {
            merge_map(&mut merged, body);
        }
        if let Some(variant) = &self.variant
            && let Some(body) = &variant.body
        {
            merge_map(&mut merged, body);
        }
        merged
    }

    /// Request headers: provider, then model, then variant.
    #[must_use]
    pub fn headers(&self) -> BTreeMap<String, String> {
        let mut merged = self.provider.headers.clone().unwrap_or_default();
        if let Some(headers) = &self.model.headers {
            merged.extend(headers.clone());
        }
        if let Some(variant) = &self.variant
            && let Some(headers) = &variant.headers
        {
            merged.extend(headers.clone());
        }
        merged
    }

    /// Effective model identifier sent on the wire.
    #[must_use]
    pub fn model_id(&self) -> &str {
        if self.model.model_id.is_empty() {
            &self.model.id
        } else {
            &self.model.model_id
        }
    }

    /// Credential from the provider's environment variables, if one is set.
    #[must_use]
    pub fn api_key_from_env(&self) -> Option<String> {
        self.provider
            .env
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    }

    /// Cost in USD for a usage record, preferring a provider-reported charge.
    #[must_use]
    pub fn cost(&self, usage: &Usage) -> f64 {
        if let Some(cost) = usage.cost {
            return cost;
        }
        calculate_cost(&self.model.cost, usage)
    }
}

/// Catalog of providers and models with a reference index.
#[derive(Debug)]
pub struct Catalog {
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    provider_index: HashMap<String, usize>,
    model_index: HashMap<(String, String), usize>,
}

impl Catalog {
    /// Parse a models.dev snapshot (v1 or v2).
    pub fn from_snapshot(json: &str) -> Result<Self, LlmError> {
        let parsed: Ordered<SourceProvider> =
            serde_json::from_str(json).map_err(|error| LlmError::Catalog {
                message: error.to_string(),
            })?;
        let providers = if parsed.0.iter().any(|(_, provider)| provider.npm.is_some()) {
            to_v2(parsed.0)
        } else {
            parsed.0
        };

        let mut catalog = Catalog {
            providers: Vec::new(),
            models: Vec::new(),
            provider_index: HashMap::new(),
            model_index: HashMap::new(),
        };

        for (key, source) in providers {
            let provider_id = if source.id.is_empty() { key } else { source.id };
            let mut routes: Vec<(SourceModel, Route)> = Vec::new();
            for (model_key, model) in source.models.0 {
                let model_id = if model.id.is_empty() {
                    model_key
                } else {
                    model.id.clone()
                };
                let mut model = model;
                if model.id.is_empty() {
                    model.id = model_id;
                }
                if !matches!(model.kind.as_str(), "chat" | "decision" | "image") {
                    continue;
                }
                if let Some(route) = select_route(&model) {
                    routes.push((model, route));
                }
            }
            if routes.is_empty() {
                continue;
            }

            let route_packages: Vec<String> = routes
                .iter()
                .map(|(_, route)| route.package.clone())
                .collect();
            let package = most_common(&route_packages).unwrap_or_default();
            let base_url = {
                let urls: Vec<Option<String>> = routes
                    .iter()
                    .map(|(_, route)| route.base_url.clone())
                    .collect();
                if urls.iter().all(Option::is_some) {
                    most_common(&urls.into_iter().flatten().collect::<Vec<_>>())
                } else {
                    None
                }
            };

            let provider = ProviderInfo {
                id: provider_id.clone(),
                canonical: None,
                name: source.name,
                activation: Activation::Auto,
                package: package.clone(),
                compatibility: None,
                settings: base_url.as_ref().map(|url| {
                    let mut map = Map::new();
                    map.insert("baseURL".to_string(), Value::String(url.clone()));
                    map
                }),
                headers: None,
                body: None,
                env: source.env,
            };

            for (source_model, route) in routes {
                let base_cost = source_model
                    .cost
                    .as_ref()
                    .map(cost_from_source)
                    .unwrap_or_default();
                let mut info = model_info(
                    &provider_id,
                    &package,
                    base_url.as_deref(),
                    &source_model,
                    &route,
                    base_cost,
                );
                let variants = variants_for(
                    &info,
                    route.protocol,
                    &ReasoningSupport::from_source(&source_model.capabilities.reasoning),
                );
                info.variants = variants;
                catalog.models.push(info);
            }
            catalog
                .provider_index
                .insert(provider.id.clone(), catalog.providers.len());
            catalog.providers.push(provider);
        }

        catalog.reindex();
        Ok(catalog)
    }

    /// Parse the embedded snapshot once and return it.
    pub fn embedded() -> Result<&'static Catalog, LlmError> {
        EMBEDDED.as_ref().map_err(|message| LlmError::Catalog {
            message: message.clone(),
        })
    }

    /// Alias for [`Catalog::embedded`].
    pub fn from_embedded() -> Result<&'static Catalog, LlmError> {
        Self::embedded()
    }

    /// Parse a snapshot and apply a merged `providers` configuration object.
    pub fn from_snapshot_with_config(json: &str, providers: &Value) -> Result<Self, LlmError> {
        let mut catalog = Self::from_snapshot(json)?;
        catalog.apply_config(providers)?;
        Ok(catalog)
    }

    #[must_use]
    pub fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    #[must_use]
    pub fn providers(&self) -> Vec<ProviderInfo> {
        self.providers.clone()
    }

    #[must_use]
    pub fn provider(&self, id: &str) -> Option<&ProviderInfo> {
        self.provider_index
            .get(id)
            .and_then(|index| self.providers.get(*index))
    }

    #[must_use]
    pub fn model(&self, provider_id: &str, id: &str) -> Option<&ModelInfo> {
        self.model_index
            .get(&(provider_id.to_string(), id.to_string()))
            .and_then(|index| self.models.get(*index))
    }

    /// Resolve `provider/id[#variant]`.
    pub fn resolve(&self, reference: &str) -> Result<ResolvedModel, LlmError> {
        let parsed = parse_reference(reference)?;
        let provider = self
            .provider(&parsed.provider)
            .ok_or_else(|| LlmError::UnknownProvider {
                provider: parsed.provider.clone(),
                candidates: self.provider_candidates(&parsed.provider),
            })?
            .clone();
        let model = self
            .model(&parsed.provider, &parsed.id)
            .ok_or_else(|| LlmError::UnknownModel {
                provider: parsed.provider.clone(),
                model: parsed.id.clone(),
                candidates: self.model_candidates(&parsed.provider, &parsed.id),
            })?
            .clone();

        let variant = parsed
            .variant
            .map(|variant_id| {
                model
                    .variants
                    .iter()
                    .find(|variant| variant.id == variant_id)
                    .cloned()
                    .ok_or_else(|| LlmError::UnknownVariant {
                        provider: parsed.provider.clone(),
                        model: parsed.id.clone(),
                        variant: variant_id,
                        available: model
                            .variants
                            .iter()
                            .map(|variant| variant.id.clone())
                            .collect(),
                    })
            })
            .transpose()?;

        let package = model.package.as_deref().unwrap_or(&provider.package);
        let protocol = if model.protocol == Protocol::Unsupported {
            protocol_for(package)
        } else {
            model.protocol
        };
        let base_url = model
            .settings
            .as_ref()
            .and_then(|settings| settings.get("baseURL"))
            .and_then(Value::as_str)
            .or_else(|| {
                provider
                    .settings
                    .as_ref()
                    .and_then(|settings| settings.get("baseURL"))
                    .and_then(Value::as_str)
            })
            .map_or_else(|| default_base_url(protocol).to_string(), str::to_string);

        Ok(ResolvedModel {
            model,
            provider,
            variant,
            protocol,
            base_url,
            reference: reference.to_string(),
        })
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

    fn provider_candidates(&self, query: &str) -> Vec<String> {
        let mut matches: Vec<&str> = self
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .filter(|id| similar(id, query))
            .collect();
        matches.sort_unstable();
        matches.truncate(5);
        matches.into_iter().map(str::to_string).collect()
    }

    fn model_candidates(&self, provider_id: &str, query: &str) -> Vec<String> {
        let mut matches: Vec<(usize, &str)> = self
            .models
            .iter()
            .filter(|model| model.provider_id == provider_id)
            .filter_map(|model| {
                let rank = similarity(&model.id, query)?;
                Some((rank, model.id.as_str()))
            })
            .collect();
        matches.sort_unstable();
        matches.truncate(5);
        matches
            .into_iter()
            .map(|(_, id)| format!("{provider_id}/{id}"))
            .collect()
    }

    fn reindex(&mut self) {
        self.provider_index.clear();
        for (index, provider) in self.providers.iter().enumerate() {
            self.provider_index.insert(provider.id.clone(), index);
        }
        self.model_index.clear();
        for (index, model) in self.models.iter().enumerate() {
            self.model_index
                .insert((model.provider_id.clone(), model.id.clone()), index);
        }
    }
}

/// Parse the reference syntax once. `provider/id[#variant]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
    pub variant: Option<String>,
}

pub fn parse_reference(reference: &str) -> Result<ModelRef, LlmError> {
    let invalid = || LlmError::InvalidReference {
        reference: reference.to_string(),
    };
    let provider_end = reference.find('/').ok_or_else(invalid)?;
    if provider_end == 0 {
        return Err(invalid());
    }
    let provider = &reference[..provider_end];
    if provider.contains('#') {
        return Err(invalid());
    }
    let rest = &reference[provider_end + 1..];
    let (id, variant) = match rest.find('#') {
        Some(hash) => (&rest[..hash], Some(&rest[hash + 1..])),
        None => (rest, None),
    };
    if id.is_empty() {
        return Err(invalid());
    }
    if let Some(variant) = variant
        && (variant.is_empty() || variant.contains('#'))
    {
        return Err(invalid());
    }
    Ok(ModelRef {
        provider: provider.to_string(),
        id: id.to_string(),
        variant: variant.map(str::to_string),
    })
}

/// Cost in USD. Mirrors upstream `SessionUsage.calculateCost`.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "token counts stay far below f64's 2^53 exact-integer range"
)]
pub fn calculate_cost(bands: &[Cost], usage: &Usage) -> f64 {
    let context = usage.non_cached_input_tokens.unwrap_or(0)
        + usage.cache_read_input_tokens.unwrap_or(0)
        + usage.cache_write_input_tokens.unwrap_or(0);
    let tier = bands
        .iter()
        .filter(|band| {
            band.tier
                .is_some_and(|tier| tier.kind == CostTierKind::Context && context > tier.size)
        })
        .max_by_key(|band| band.tier.map_or(0, |tier| tier.size));
    let band = tier.or_else(|| bands.iter().find(|band| band.tier.is_none()));
    let Some(band) = band else {
        return 0.0;
    };
    let safe = |value: Option<u64>| value.unwrap_or(0) as f64;
    let visible_output = usage.visible_output_tokens();
    (safe(usage.non_cached_input_tokens) * band.input
        + (visible_output as f64 + safe(usage.reasoning_tokens)) * band.output
        + safe(usage.cache_read_input_tokens) * band.cache.read
        + safe(usage.cache_write_input_tokens) * band.cache.write)
        / 1_000_000.0
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "catalog limits are non-negative token counts; fractional/NaN input clamps to zero"
)]
fn count(value: f64) -> u64 {
    if value.is_finite() {
        value.max(0.0) as u64
    } else {
        0
    }
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value.max(0)).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Snapshot source schema
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SourceProvider {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    env: Vec<String>,
    /// models.dev v1 only.
    #[serde(default)]
    npm: Option<String>,
    /// models.dev v1 only.
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: Ordered<SourceModel>,
}

#[derive(Debug, Deserialize)]
struct SourceModel {
    #[serde(default)]
    id: String,
    #[serde(rename = "type", default = "chat_type")]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    modalities: SourceModalities,
    #[serde(default)]
    capabilities: SourceCapabilities,
    #[serde(default)]
    limit: Option<SourceLimit>,
    #[serde(default)]
    cost: Option<SourceCost>,
    #[serde(default)]
    api: Ordered<SourceApi>,
    // models.dev v1 fields.
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    reasoning_options: Vec<SourceReasoningOption>,
    #[serde(default)]
    temperature: Option<bool>,
    #[serde(default)]
    interleaved: Option<Value>,
    #[serde(default)]
    provider: Option<SourceModelProvider>,
}

fn chat_type() -> String {
    "chat".to_string()
}

#[derive(Debug, Default, Deserialize)]
struct SourceModelProvider {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    shape: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceModalities {
    #[serde(default = "text_modalities")]
    input: Vec<String>,
    #[serde(default = "text_modalities")]
    output: Vec<String>,
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
struct SourceCapabilities {
    #[serde(default)]
    tools: SourceTools,
    #[serde(default)]
    reasoning: SourceReasoning,
    #[serde(default)]
    temperature: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceTools {
    #[serde(default)]
    supported: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct SourceReasoning {
    #[serde(default)]
    supported: bool,
    #[serde(default)]
    toggle: bool,
    #[serde(default)]
    effort: Option<Vec<String>>,
    #[serde(default)]
    budget: Option<SourceBudget>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceBudget {
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceReasoningOption {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    values: Option<Vec<Option<String>>>,
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct SourceLimit {
    #[serde(default)]
    context: Option<f64>,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceApi {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    sdk: Option<SourceSdk>,
    #[serde(default)]
    compatibility: Option<SourceCompatibility>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceSdk {
    #[serde(default)]
    aisdk: Option<String>,
    #[serde(default)]
    opencodeai: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceCompatibility {
    #[serde(default)]
    reasoning_field: Option<String>,
    #[serde(default)]
    max_tokens_field: Option<String>,
    #[serde(default)]
    media_input: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceCost {
    #[serde(default)]
    text: Option<SourceTextCost>,
    // models.dev v1 cost fields share the same object.
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    #[serde(default)]
    tiers: Vec<SourceV1Tier>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceTextCost {
    #[serde(default)]
    input: Option<SourcePrice>,
    #[serde(default)]
    output: Option<SourcePrice>,
}

#[derive(Debug, Default, Deserialize)]
struct SourcePrice {
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    price: f64,
    #[serde(default)]
    cache: Option<SourceCachePrices>,
    #[serde(default)]
    overrides: Vec<SourceOverride>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceCachePrices {
    #[serde(default)]
    read: Option<f64>,
    #[serde(default)]
    write: Option<FirstPrice>,
    #[serde(default)]
    explicit: Option<SourceExplicitCache>,
}

#[derive(Debug, Default, Deserialize)]
struct SourceExplicitCache {
    #[serde(default)]
    write: Option<FirstPrice>,
}

/// A cache-write price is either a number or a TTL map; upstream keeps the first entry.
#[derive(Debug, Default)]
struct FirstPrice(Option<f64>);

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
struct SourceOverride {
    #[serde(default)]
    when: SourceWhen,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    cache: Option<SourceCachePrices>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct SourceWhen {
    #[serde(default)]
    context: Option<SourceContext>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct SourceContext {
    #[serde(default)]
    gt: Option<f64>,
    #[serde(default)]
    gte: Option<f64>,
}

#[derive(Debug, Clone)]
struct Route {
    package: String,
    base_url: Option<String>,
    compatibility: Option<Compatibility>,
    protocol: Protocol,
}

fn select_route(model: &SourceModel) -> Option<Route> {
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

fn protocol_for_route(api_name: &str, package: &str) -> Protocol {
    match api_name {
        "chat-completions" => Protocol::OpenAiChat,
        "messages" => Protocol::AnthropicMessages,
        "generate-content" => Protocol::Gemini,
        "responses" | "converse" => Protocol::Unsupported,
        _ => protocol_for(package),
    }
}

fn protocol_for(package: &str) -> Protocol {
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

fn default_base_url(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiChat => "https://api.openai.com/v1",
        Protocol::AnthropicMessages => "https://api.anthropic.com/v1",
        Protocol::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        Protocol::Unsupported => "",
    }
}

fn model_info(
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

fn cost_from_source(input: &SourceCost) -> Vec<Cost> {
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

fn cost_from_config(value: &Value) -> Result<Vec<Cost>, LlmError> {
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

fn compatibility_from_value(value: &Map<String, Value>) -> Compatibility {
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

// ---------------------------------------------------------------------------
// v1 -> v2 conversion
// ---------------------------------------------------------------------------

fn to_v2(providers: Vec<(String, SourceProvider)>) -> Vec<(String, SourceProvider)> {
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
struct SourceV1Tier {
    tier: SourceV1TierMeta,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct SourceV1TierMeta {
    size: u64,
}

impl From<f64> for FirstPrice {
    fn from(value: f64) -> Self {
        FirstPrice(Some(value))
    }
}

// ---------------------------------------------------------------------------
// Variants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct ReasoningSupport {
    toggle: bool,
    effort: Option<Vec<String>>,
    budget: Option<(Option<i64>, Option<i64>)>,
}

impl ReasoningSupport {
    fn from_source(source: &SourceReasoning) -> Self {
        if !source.supported {
            return Self::default();
        }
        Self {
            toggle: source.toggle,
            effort: source.effort.clone(),
            budget: source
                .budget
                .as_ref()
                .map(|budget| (budget.min, budget.max)),
        }
    }
}

fn reasoning_support_from_config(value: &Value) -> ReasoningSupport {
    let Some(object) = value.as_object() else {
        return ReasoningSupport::default();
    };
    if object.get("supported").and_then(Value::as_bool) != Some(true) {
        return ReasoningSupport::default();
    }
    ReasoningSupport {
        toggle: object
            .get("toggle")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        effort: object
            .get("effort")
            .and_then(Value::as_array)
            .map(|values| strings(values))
            .map(|values| {
                values
                    .into_iter()
                    .filter(|value| value != "default")
                    .collect()
            }),
        budget: object
            .get("budget")
            .and_then(Value::as_object)
            .map(|budget| {
                (
                    budget.get("min").and_then(Value::as_i64),
                    budget.get("max").and_then(Value::as_i64),
                )
            }),
    }
}

fn variants_for(model: &ModelInfo, protocol: Protocol, support: &ReasoningSupport) -> Vec<Variant> {
    match protocol {
        Protocol::OpenAiChat => openai_variants(model, support),
        Protocol::AnthropicMessages => anthropic_variants(model, support),
        Protocol::Gemini => gemini_variants(model, support),
        Protocol::Unsupported => Vec::new(),
    }
}

fn settings_variant(id: &str, settings: &Value) -> Variant {
    Variant {
        id: id.to_string(),
        settings: settings.as_object().cloned(),
        headers: None,
        body: None,
    }
}

fn with_settings(id: &str, inner: Map<String, Value>) -> Variant {
    settings_variant(id, &Value::Object(inner))
}

fn openai_variants(_model: &ModelInfo, support: &ReasoningSupport) -> Vec<Variant> {
    let Some(values) = support.effort.as_ref() else {
        return Vec::new();
    };
    let values = if values.is_empty() {
        vec!["low".to_string(), "medium".to_string(), "high".to_string()]
    } else {
        values.clone()
    };
    values
        .into_iter()
        .map(|effort| {
            with_settings(
                &effort,
                Map::from_iter([("reasoningEffort".to_string(), Value::String(effort.clone()))]),
            )
        })
        .collect()
}

#[derive(Debug)]
struct ClaudeInfo {
    family: Option<String>,
    major: u64,
    minor: u64,
    manual: bool,
    always: bool,
}

fn claude_info(model: &ModelInfo) -> ClaudeInfo {
    let id = model.model_id.to_ascii_lowercase();
    let families = ["opus", "sonnet", "haiku", "fable", "mythos"];
    let mut family = None;
    let mut major = 0;
    let mut minor = 0;
    let tokens: Vec<&str> = id
        .split(|character: char| !character.is_ascii_alphanumeric())
        .collect();
    for (index, token) in tokens.iter().enumerate() {
        if !families.contains(token) {
            continue;
        }
        family = Some((*token).to_string());
        // family-first: claude-sonnet-4-5
        if let Some(number) = tokens
            .get(index + 1)
            .and_then(|token| token.parse::<u64>().ok())
        {
            major = number;
            minor = tokens
                .get(index + 2)
                .and_then(|token| token.parse().ok())
                .unwrap_or(0);
        // version-first: claude-4-5-sonnet
        } else if let (Some(major_token), Some(minor_token)) = (
            index.checked_sub(2).and_then(|index| tokens.get(index)),
            index.checked_sub(1).and_then(|index| tokens.get(index)),
        ) && let (Ok(major_value), Ok(minor_value)) =
            (major_token.parse::<u64>(), minor_token.parse::<u64>())
        {
            major = major_value;
            minor = minor_value;
        }
        break;
    }
    let manual = (major == 3 && minor == 7) || (major == 4 && minor < 6);
    let always = family.as_deref() == Some("fable")
        || family.as_deref() == Some("mythos")
        || id.contains("mythos-preview");
    ClaudeInfo {
        family,
        major,
        minor,
        manual,
        always,
    }
}

fn manual_thinking(model: &ModelInfo) -> Option<Map<String, Value>> {
    let tokens = model.limit.output.min(16_000).saturating_sub(1);
    if tokens < 1024 {
        return None;
    }
    Some(Map::from_iter([(
        "thinking".to_string(),
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("enabled".to_string())),
            ("budgetTokens".to_string(), Value::Number(tokens.into())),
        ])),
    )]))
}

fn toggle_variants(off: &Value, on: &Value) -> Vec<Variant> {
    vec![
        settings_variant("none", off),
        settings_variant("thinking", on),
    ]
}

fn budget_variants(
    model: &ModelInfo,
    budget: (Option<i64>, Option<i64>),
    ceiling: u64,
    build: impl Fn(u64) -> Value,
) -> Vec<Variant> {
    let output_ceiling = model.limit.output.max(1);
    let maximum = budget
        .1
        .map_or(ceiling.saturating_sub(1), unsigned)
        .min(output_ceiling.saturating_sub(1))
        .min(ceiling.saturating_sub(1));
    if maximum == 0 {
        return Vec::new();
    }
    let high = unsigned(budget.0.unwrap_or(0))
        .max(maximum.div_ceil(2))
        .min(maximum);
    vec![
        settings_variant("high", &build(high)),
        settings_variant("max", &build(maximum)),
    ]
}

#[allow(
    clippy::too_many_lines,
    reason = "Claude effort/budget/toggle variants are a single precedence table"
)]
fn anthropic_variants(model: &ModelInfo, support: &ReasoningSupport) -> Vec<Variant> {
    let info = claude_info(model);
    if info.family.is_none() {
        if let Some(values) = &support.effort {
            let values = if values.is_empty() {
                vec!["low".to_string(), "medium".to_string(), "high".to_string()]
            } else {
                values.clone()
            };
            return values
                .into_iter()
                .map(|effort| {
                    with_settings(
                        &effort,
                        Map::from_iter([(
                            "thinking".to_string(),
                            serde_json::json!({"type": "adaptive"}),
                        )]),
                    )
                })
                .collect();
        }
        if support.toggle {
            return toggle_variants(
                &serde_json::json!({"thinking": {"type": "disabled"}}),
                &serde_json::json!({"thinking": {"type": "adaptive"}}),
            );
        }
        return Vec::new();
    }

    if let Some(values) = &support.effort {
        if info.manual
            && !(info.family.as_deref() == Some("opus") && info.major == 4 && info.minor == 5)
        {
            return anthropic_variants(
                model,
                &ReasoningSupport {
                    budget: Some((Some(1024), None)),
                    ..ReasoningSupport::default()
                },
            );
        }
        let defaults: Vec<String> = if info.major == 4 && info.minor == 6 {
            ["low", "medium", "high", "max"]
                .map(str::to_string)
                .to_vec()
        } else {
            ["low", "medium", "high", "xhigh", "max"]
                .map(str::to_string)
                .to_vec()
        };
        let values = if values.is_empty() {
            defaults
        } else {
            values.clone()
        };
        let opus45 = info.family.as_deref() == Some("opus") && info.major == 4 && info.minor == 5;
        return values
            .into_iter()
            .filter(|effort| !(info.always && effort == "none"))
            .map(|effort| {
                if effort == "none" {
                    return settings_variant(
                        "none",
                        &serde_json::json!({"thinking": {"type": "disabled"}}),
                    );
                }
                let mut settings = if opus45 {
                    manual_thinking(model).unwrap_or_else(|| {
                        Map::from_iter([(
                            "thinking".to_string(),
                            serde_json::json!({"type": "enabled"}),
                        )])
                    })
                } else {
                    Map::from_iter([(
                        "thinking".to_string(),
                        serde_json::json!({"type": "adaptive", "display": "summarized"}),
                    )])
                };
                settings.insert("effort".to_string(), Value::String(effort.clone()));
                with_settings(&effort, settings)
            })
            .collect();
    }

    if support.toggle {
        if info.always {
            return Vec::new();
        }
        let thinking = if info.manual {
            manual_thinking(model).unwrap_or_else(|| {
                Map::from_iter([(
                    "thinking".to_string(),
                    serde_json::json!({"type": "enabled"}),
                )])
            })
        } else {
            Map::from_iter([(
                "thinking".to_string(),
                serde_json::json!({"type": "adaptive", "display": "summarized"}),
            )])
        };
        return toggle_variants(
            &serde_json::json!({"thinking": {"type": "disabled"}}),
            &Value::Object(thinking),
        );
    }

    if let Some(budget) = support.budget {
        return budget_variants(
            model,
            budget,
            32_000,
            |tokens| serde_json::json!({"thinking": {"type": "enabled", "budgetTokens": tokens}}),
        );
    }
    Vec::new()
}

fn gemini_variants(model: &ModelInfo, support: &ReasoningSupport) -> Vec<Variant> {
    if let Some(values) = &support.effort {
        let values = if values.is_empty() {
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        } else {
            values.clone()
        };
        return values
            .into_iter()
            .map(|effort| {
                with_settings(
                    &effort,
                    serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": effort}})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
    }
    if let Some(budget) = support.budget {
        return budget_variants(
            model,
            budget,
            model.limit.output.max(1),
            |tokens| serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": tokens}}),
        );
    }
    if support.toggle {
        return toggle_variants(
            &serde_json::json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}),
            &serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}}),
        );
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn merge_map(base: &mut Map<String, Value>, overlay: &Map<String, Value>) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(Value::Object(base_object)), Value::Object(overlay_object)) => {
                merge_map(base_object, overlay_object);
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn most_common<T: Clone + Eq>(values: &[T]) -> Option<T> {
    let mut counts: Vec<(&T, usize)> = Vec::new();
    for value in values {
        if let Some(entry) = counts.iter_mut().find(|(candidate, _)| *candidate == value) {
            entry.1 += 1;
        } else {
            counts.push((value, 1));
        }
    }
    let mut best: Option<(&T, usize)> = None;
    for entry in counts {
        if best.is_none_or(|(_, count)| entry.1 > count) {
            best = Some(entry);
        }
    }
    best.map(|(value, _)| value.clone())
}

fn strings(values: &[Value]) -> Vec<String> {
    values
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn similar(candidate: &str, query: &str) -> bool {
    similarity(candidate, query).is_some()
}

fn similarity(candidate: &str, query: &str) -> Option<usize> {
    let candidate_lower = candidate.to_ascii_lowercase();
    let query_lower = query.to_ascii_lowercase();
    if candidate_lower == query_lower {
        Some(0)
    } else if candidate_lower.starts_with(&query_lower) {
        Some(1)
    } else if candidate_lower.contains(&query_lower) || query_lower.contains(&candidate_lower) {
        Some(2)
    } else {
        None
    }
}

/// Parse `YYYY-MM-DD` / `YYYY-MM` / RFC 3339 into epoch milliseconds. Invalid input is `0`.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "numeric release timestamps are milliseconds and bounded by epoch ranges"
)]
pub fn parse_release_date(value: &str) -> i64 {
    if let Ok(seconds) = value.parse::<f64>() {
        return (seconds * 1000.0) as i64;
    }
    let date = value.split_once('T').map_or(value, |(date, _)| date).trim();
    let mut parts = date.split('-');
    let (Some(year), Some(month)) = (parts.next(), parts.next()) else {
        return 0;
    };
    let (Ok(year), Ok(month)) = (year.parse::<i64>(), month.parse::<i64>()) else {
        return 0;
    };
    let day = parts
        .next()
        .and_then(|day| day.parse::<i64>().ok())
        .unwrap_or(1);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    days_from_civil(year, month, day) * 86_400_000
}

// Howard Hinnant's days_from_civil.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_rejects_bad_syntax() {
        for reference in [
            "",
            "provider",
            "/model",
            "provider/",
            "provider/#x",
            "pro#vider/x",
        ] {
            assert!(parse_reference(reference).is_err(), "{reference}");
        }
        assert_eq!(
            parse_reference("anthropic/claude#high").unwrap(),
            ModelRef {
                provider: "anthropic".to_string(),
                id: "claude".to_string(),
                variant: Some("high".to_string()),
            }
        );
    }

    #[test]
    fn date_parses_to_millis() {
        assert_eq!(parse_release_date("1970-01-01"), 0);
        assert_eq!(parse_release_date("2025-09-29"), 1_759_104_000_000);
    }
}
