//! A swappable catalog handle.
//!
//! A live refresh replaces the whole catalog, but request handlers and the
//! runtime hold it for the duration of a turn. [`CatalogCell`] keeps one
//! `Arc<Catalog>` behind a read-write lock: readers clone the `Arc` and never
//! observe a half-merged catalog, and a writer swaps in a fully parsed one.
//!
//! The revision counter lets a caller cheaply detect that a refresh happened
//! without comparing catalogs.

use std::sync::{Arc, RwLock};

use super::Catalog;

/// Snapshot-on-read holder for the process-wide catalog.
#[derive(Debug)]
pub struct CatalogCell {
    current: RwLock<Arc<Catalog>>,
    revision: RwLock<u64>,
}

impl CatalogCell {
    /// Wrap an initial catalog at revision 0.
    #[must_use]
    pub fn new(catalog: Catalog) -> Self {
        Self {
            current: RwLock::new(Arc::new(catalog)),
            revision: RwLock::new(0),
        }
    }

    /// Wrap an already-shared catalog.
    #[must_use]
    pub fn from_arc(catalog: Arc<Catalog>) -> Self {
        Self {
            current: RwLock::new(catalog),
            revision: RwLock::new(0),
        }
    }

    /// The catalog a caller should use for this request or turn.
    ///
    /// The returned `Arc` keeps that generation alive even if a refresh swaps
    /// in a newer one mid-turn, so a turn cannot mix two catalog generations.
    #[must_use]
    pub fn current(&self) -> Arc<Catalog> {
        Arc::clone(
            &self
                .current
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Replace the catalog and bump the revision.
    pub fn replace(&self, catalog: Catalog) {
        self.replace_arc(Arc::new(catalog));
    }

    /// Replace the catalog with an already-shared one and bump the revision.
    pub fn replace_arc(&self, catalog: Arc<Catalog>) {
        {
            let mut current = self
                .current
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current = catalog;
        }
        let mut revision = self
            .revision
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *revision = revision.wrapping_add(1);
    }

    /// Mutate the live catalog in place, keeping one generation.
    ///
    /// A merge that only adds models is applied this way: readers before the
    /// write see the old catalog and readers after see the merged one; there is
    /// no window where the catalog is `None` or partial.
    pub fn update(&self, apply: impl FnOnce(&mut Catalog)) {
        {
            let mut current = self
                .current
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let catalog = Arc::make_mut(&mut current);
            apply(catalog);
        }
        let mut revision = self
            .revision
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *revision = revision.wrapping_add(1);
    }

    /// Provider metadata by id, cloned so the caller holds no lock.
    #[must_use]
    pub fn provider(&self, id: &str) -> Option<super::ProviderInfo> {
        self.current().provider(id).cloned()
    }

    /// One model by provider and id, cloned so the caller holds no lock.
    #[must_use]
    pub fn model(&self, provider_id: &str, id: &str) -> Option<super::ModelInfo> {
        self.current().model(provider_id, id).cloned()
    }

    /// Every model in the current generation.
    #[must_use]
    pub fn models(&self) -> Vec<super::ModelInfo> {
        self.current().models()
    }

    /// Every provider in the current generation.
    #[must_use]
    pub fn providers(&self) -> Vec<super::ProviderInfo> {
        self.current().providers()
    }

    /// Monotonic revision; changes exactly when the catalog changed.
    #[must_use]
    pub fn revision(&self) -> u64 {
        *self
            .revision
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"{
      "p": {
        "id": "p",
        "name": "P",
        "env": [],
        "models": {
          "a": {
            "id": "a",
            "type": "chat",
            "name": "A",
            "api": {"chat-completions": {"sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
          }
        }
      }
    }"#;

    const EXTRA: &str = r#"{
      "p": {
        "id": "p",
        "name": "P",
        "env": [],
        "models": {
          "b": {
            "id": "b",
            "type": "chat",
            "name": "B",
            "api": {"chat-completions": {"sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
          }
        }
      }
    }"#;

    #[test]
    fn update_is_visible_to_the_next_reader_and_bumps_the_revision() {
        let cell = CatalogCell::new(Catalog::from_snapshot(CATALOG).unwrap());
        assert_eq!(cell.revision(), 0);
        // A handle taken before the update keeps the old generation.
        let before = cell.current();
        assert!(before.resolve("p/b").is_err());

        cell.update(|catalog| {
            crate::catalog::refresh::merge_live(catalog, EXTRA).expect("merge");
        });

        assert_eq!(cell.revision(), 1);
        assert!(cell.current().resolve("p/b").is_ok());
        // The pre-update snapshot is unchanged: a turn cannot mix generations.
        assert!(before.resolve("p/b").is_err());
    }

    #[test]
    fn replace_swaps_the_whole_catalog() {
        let cell = CatalogCell::new(Catalog::from_snapshot(CATALOG).unwrap());
        cell.replace(Catalog::from_snapshot(EXTRA).unwrap());
        assert_eq!(cell.revision(), 1);
        assert!(cell.current().resolve("p/a").is_err());
        assert!(cell.current().resolve("p/b").is_ok());
    }
}
