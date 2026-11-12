//! Retrieval and write paths for permanent memory.
//!
//! [`MemoryService`] wraps [`oc_store::Store`]: retrieval merges the caller's
//! visible scopes, ranks pinned rows first, removes exact and near duplicates,
//! then renders a `<memory>` block trimmed to a token budget. Writes apply the
//! 2000-character cap, the secret detector and the documented defaults.
//!
//! The facade owns the request/response types and the service handle; [search]
//! renders retrieval, [write] the write and forget paths, [list] the offset
//! pagination, [touch] usage accounting, [scope] the scope vocabulary, [rank]
//! the ordering and deduplication, and [input] shared normalization.

use std::fmt;
use std::sync::Arc;

use oc_store::Store;

mod input;
mod list;
mod rank;
mod scope;
mod search;
mod touch;
mod write;

#[cfg(test)]
mod tests;

/// Maximum stored text length, in characters.
pub const TEXT_CAP: usize = 2000;
/// Default importance for writes that omit it.
pub const DEFAULT_IMPORTANCE: u8 = 3;
/// Default confidence for writes that omit it.
pub const DEFAULT_CONFIDENCE: f32 = 0.7;
/// Importance at which a memory is pinned into every retrieval.
pub const PINNED_IMPORTANCE: u8 = 5;
/// Retrieval never asks the store for fewer than this many rows.
pub const MIN_SEARCH_LIMIT: usize = 12;
/// Upper bound accepted by the store's search.
pub const MAX_SEARCH_LIMIT: usize = 50;
/// Default page size for [`MemoryService::list`].
pub const LIST_DEFAULT: usize = 20;
/// Maximum page size for [`MemoryService::list`].
pub const LIST_MAX: usize = 200;

/// One retrieval request: visible scopes, query text and rendering budget.
#[derive(Debug, Clone, Default)]
pub struct RetrieveRequest {
    /// Visible scopes as `(scope, scope_id)`, most-shared first.
    pub scopes: Vec<(String, Option<String>)>,
    /// Free-text query; empty means "rank everything visible".
    pub query: String,
    /// Restrict to these kinds.
    pub kinds: Vec<String>,
    /// Require at least one of these tags.
    pub tags: Vec<String>,
    /// Requested result count; the service asks for at least 12.
    pub limit: usize,
    /// Estimated token budget; `0` uses [`crate::render::DEFAULT_TOKEN_BUDGET`].
    pub token_budget: usize,
}

/// Rendered memory block plus the ids it contains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryBlock {
    /// `<memory>` block, or empty when nothing was retrieved.
    pub text: String,
    /// Ids rendered, in block order.
    pub ids: Vec<String>,
    /// Estimated tokens (`chars / 4`) of `text`.
    pub tokens: u64,
}

/// A memory to write. Defaults match the memory contract: kind `fact`, scope
/// `project`, importance 3, confidence 0.7, source `agent`.
#[derive(Debug, Clone, Default)]
pub struct NewMemoryInput {
    /// Memory text; required, trimmed, at most [`TEXT_CAP`] characters.
    pub text: String,
    /// One of `fact`, `decision`, `preference`, `pattern`, `gotcha`.
    pub kind: Option<String>,
    /// One of `global`, `project`, `agent`, `session`.
    pub scope: Option<String>,
    /// Project / agent / session id; ignored for `global`.
    pub scope_id: Option<String>,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// 1..=5; the store clamps.
    pub importance: Option<u8>,
    /// 0.0..=1.0; the store clamps.
    pub confidence: Option<f32>,
    /// `user`, `agent`, `scribe` or `import`.
    pub source: Option<String>,
    /// Session that produced the memory.
    pub source_session: Option<String>,
    /// Message that produced the memory.
    pub source_message: Option<String>,
}

/// A list request. `cursor` is the decimal offset returned with the previous
/// page (the tools expose it as `cursor=<n>`).
#[derive(Debug, Clone, Default)]
pub struct ListRequest {
    /// Visible scopes as `(scope, scope_id)`, most-shared first.
    pub scopes: Vec<(String, Option<String>)>,
    /// Restrict to this kind.
    pub kind: Option<String>,
    /// Require at least one of these tags.
    pub tags: Vec<String>,
    /// Page size; `0` uses [`LIST_DEFAULT`].
    pub limit: usize,
    /// Offset cursor from a previous page.
    pub cursor: Option<String>,
}

/// Memory reads, writes and list operations over one store.
#[derive(Clone)]
pub struct MemoryService {
    pub(super) store: Arc<Store>,
}

impl fmt::Debug for MemoryService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryService")
            .finish_non_exhaustive()
    }
}

impl MemoryService {
    /// Wraps a store.
    #[must_use]
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}
