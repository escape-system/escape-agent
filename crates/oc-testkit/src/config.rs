//! Test configuration generation.

use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};

/// Configuration written to `OPENCODE_CONFIG` before the server starts.
///
/// A default config is empty; [`TestConfig::with_provider`] adds the
/// `providers.fake` overlay plus the matching default model.
#[derive(Debug, Clone, Default)]
pub struct TestConfig {
    providers: Map<String, Value>,
    model: Option<String>,
    default_agent: Option<String>,
    permissions: Vec<Value>,
    env: Vec<(String, String)>,
}

impl TestConfig {
    #[must_use]
    pub fn new() -> Self {
        Self {
            default_agent: Some("build".to_owned()),
            ..Self::default()
        }
    }

    /// Register `providers.fake` targeting `base_url` using OpenAI-compatible
    /// chat completions and select `fake/model` as the default model.
    #[must_use]
    pub fn with_provider(self, base_url: &str) -> Self {
        self.with_provider_package(base_url, "@opencode/ai/providers/openai-compatible")
    }

    /// Register `providers.fake` targeting `base_url` with Anthropic Messages
    /// protocol and select `fake/model` as the default model.
    #[must_use]
    pub fn with_anthropic_provider(self, base_url: &str) -> Self {
        self.with_provider_package(base_url, "@ai-sdk/anthropic")
    }

    /// Register `providers.fake` targeting `base_url` with Gemini
    /// `streamGenerateContent` protocol and select `fake/model` as the default
    /// model.
    #[must_use]
    pub fn with_gemini_provider(self, base_url: &str) -> Self {
        self.with_provider_package(base_url, "@ai-sdk/google")
    }

    /// Register `providers.fake` targeting `base_url` and `package`, and select
    /// `fake/model` as the default model.
    #[must_use]
    pub fn with_provider_package(mut self, base_url: &str, package: &str) -> Self {
        let model = json!({
            "id": "model",
            "name": "Fake Model",
            "type": "chat",
            "limit": {"context": 200_000, "output": 8192},
            "capabilities": {
                "tools": true,
                "input": ["text"],
                "output": ["text"],
            },
            "settings": {"baseURL": base_url},
        });
        let provider = json!({
            "name": "Fake Provider",
            "package": package,
            "activation": "enabled",
            "settings": {"baseURL": base_url},
            "models": {"model": model},
        });
        self.providers.insert("fake".to_owned(), provider);
        self.model = Some("fake/model".to_owned());
        self
    }

    /// Append a document-level permission rule.
    #[must_use]
    pub fn with_permission(mut self, action: &str, resource: &str, effect: &str) -> Self {
        self.permissions
            .push(json!({"action": action, "resource": resource, "effect": effect}));
        self
    }

    /// Add an environment variable for the server child process.
    #[must_use]
    pub fn with_env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_owned(), value.into()));
        self
    }

    /// Environment pairs configured by the test.
    #[must_use]
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// Render the configuration document.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut document = Map::new();
        if let Some(model) = &self.model {
            document.insert("model".to_owned(), Value::String(model.clone()));
        }
        if let Some(agent) = &self.default_agent {
            document.insert("default_agent".to_owned(), Value::String(agent.clone()));
        }
        if !self.providers.is_empty() {
            document.insert(
                "providers".to_owned(),
                Value::Object(self.providers.clone()),
            );
        }
        if !self.permissions.is_empty() {
            document.insert(
                "permissions".to_owned(),
                Value::Array(self.permissions.clone()),
            );
        }
        Value::Object(document)
    }

    /// Write the configuration to `path`.
    ///
    /// # Errors
    ///
    /// Returns any filesystem or serialization error.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.to_value()).map_err(io::Error::other)?;
        std::fs::write(path, bytes)
    }
}
