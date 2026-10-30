//! Resolved model view: merged settings, headers, body, and pricing.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::event::Usage;

use super::cost::calculate_cost;
use super::types::{ModelInfo, Protocol, ProviderInfo, Variant};
use super::util::merge_map;

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
