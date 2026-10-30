//! Reference resolution, accessors, and unknown-id candidates.

use serde_json::Value;

use crate::error::LlmError;

use super::Catalog;
use super::EMBEDDED;
use super::convert::{default_base_url, protocol_for};
use super::reference::parse_reference;
use super::resolved::ResolvedModel;
use super::types::{ModelInfo, Protocol, ProviderInfo};
use super::util::{similar, similarity};

impl Catalog {
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
}
