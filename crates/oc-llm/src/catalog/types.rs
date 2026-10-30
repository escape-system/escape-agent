//! Catalog data types shared by snapshot parsing, overlays, and resolution.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
    pub(super) fn merge(&mut self, overlay: &Compatibility) {
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
