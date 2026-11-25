//! Snapshot parsing and provider/model aggregation into a catalog.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::error::LlmError;

use super::Catalog;
use super::Ordered;
use super::convert::{cost_from_source, model_info};
use super::source::{Route, SourceModel, SourceProvider, select_route};
use super::types::{Activation, ProviderInfo};
use super::util::most_common;
use super::v1::to_v2;
use super::variants::{ReasoningSupport, variants_for};

fn intern_settings(
    intern: &mut HashMap<String, Arc<Map<String, Value>>>,
    slot: &mut Option<Arc<Map<String, Value>>>,
) {
    let Some(current) = slot.as_ref() else {
        return;
    };
    if let Ok(key) = serde_json::to_string(current.as_ref()) {
        if let Some(shared) = intern.get(&key) {
            *slot = Some(Arc::clone(shared));
        } else {
            intern.insert(key, Arc::clone(current));
        }
    }
}

fn common_base_url(routes: &[(SourceModel, Route)]) -> Option<String> {
    let urls: Vec<&str> = routes
        .iter()
        .map(|(_, route)| route.base_url.as_deref())
        .collect::<Option<Vec<&str>>>()?;
    most_common(&urls).map(str::to_string)
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

        let mut intern: HashMap<String, Arc<Map<String, Value>>> = HashMap::new();

        let mut catalog = Catalog {
            providers: Vec::new(),
            models: Vec::new(),
            sorted_providers: Vec::new(),
            sorted_models: Vec::new(),
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
            let base_url = common_base_url(&routes);

            let mut provider = ProviderInfo {
                id: provider_id.clone(),
                canonical: None,
                name: source.name,
                activation: Activation::Auto,
                package: package.clone(),
                compatibility: None,
                settings: base_url.as_ref().map(|url| {
                    let mut map = Map::new();
                    map.insert("baseURL".to_string(), Value::String(url.clone()));
                    Arc::new(map)
                }),
                headers: None,
                body: None,
                env: source.env,
            };
            intern_settings(&mut intern, &mut provider.settings);

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
                let mut variants = variants_for(
                    &info,
                    route.protocol,
                    &ReasoningSupport::from_source(&source_model.capabilities.reasoning),
                );
                for variant in &mut variants {
                    intern_settings(&mut intern, &mut variant.settings);
                }
                intern_settings(&mut intern, &mut info.settings);
                info.variants = variants;
                catalog.models.push(info);
            }
            catalog.providers.push(provider);
        }

        catalog.reindex();
        Ok(catalog)
    }
}
