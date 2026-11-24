//! Bundled models.dev catalog, model resolution, and configuration overlays.
//!
//! The snapshot is embedded at build time, parsed once on first use, and never
//! performs network or file I/O. Configuration is supplied as an already-merged
//! `providers` JSON object.

mod cell;
mod convert;
mod cost;
mod overlay;
mod reference;
pub mod refresh;
mod resolve;
mod resolved;
mod snapshot;
mod source;
#[cfg(test)]
mod tests;
mod time;
mod types;
mod util;
mod v1;
mod variants;

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub use cell::CatalogCell;
pub use cost::calculate_cost;
pub use reference::{ModelRef, parse_reference};
pub use resolved::ResolvedModel;
pub use time::parse_release_date;
pub use types::{
    Activation, Capabilities, Compatibility, Cost, CostCache, CostTier, CostTierKind, Limit,
    ModelInfo, ModelStatus, ModelTime, PriceOverride, PriceOverrideMap, Protocol, ProviderInfo,
    Variant,
};

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

/// Catalog of providers and models with a reference index.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    provider_index: HashMap<String, usize>,
    model_index: HashMap<(String, String), usize>,
}

impl Catalog {
    /// Insert or replace a provider definition while preserving index lookups.
    pub fn upsert_provider(&mut self, provider: ProviderInfo) {
        if let Some(&index) = self.provider_index.get(&provider.id) {
            self.providers[index] = provider;
        } else {
            let index = self.providers.len();
            self.provider_index.insert(provider.id.clone(), index);
            self.providers.push(provider);
        }
    }

    /// Insert or replace models for `provider_id`, keeping lookup indices synced.
    pub fn upsert_provider_models(&mut self, models: Vec<ModelInfo>) {
        for model in models {
            let key = (model.provider_id.clone(), model.id.clone());
            if let Some(&index) = self.model_index.get(&key) {
                self.models[index] = model;
            } else {
                let index = self.models.len();
                self.model_index.insert(key, index);
                self.models.push(model);
            }
        }
    }

    /// Drop every model of `provider_id` whose id is absent from `keep`.
    ///
    /// A provider that publishes its own model list is authoritative for
    /// **membership**: a model the subscription no longer includes must stop
    /// being offered, even though `models.dev` still lists it (verified live:
    /// `space-bunny` answers `isn't included in OpenCode Go`, `glm-5.2` answers
    /// `has been deprecated`). Callers gate this on
    /// [`refresh::is_plausible_provider_list`] so a truncated response never
    /// empties the provider. Returns the number of models removed.
    pub fn retain_provider_models(
        &mut self,
        provider_id: &str,
        keep: &std::collections::HashSet<String>,
    ) -> usize {
        let before = self.models.len();
        self.models
            .retain(|model| model.provider_id != provider_id || keep.contains(&model.id));
        let removed = before - self.models.len();
        if removed > 0 {
            self.reindex();
        }
        removed
    }
}
