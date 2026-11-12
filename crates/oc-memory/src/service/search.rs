//! Retrieval: validate the visible scopes, search, rank and render a block.

use crate::error::MemoryError;
use crate::render::{DEFAULT_TOKEN_BUDGET, normalize_line, render_block};
use oc_store::MemoryQuery;

use super::rank::rank_and_dedupe;
use super::scope::{block_scope, validate_scope};
use super::{
    MAX_SEARCH_LIMIT, MIN_SEARCH_LIMIT, MemoryBlock, MemoryService, PINNED_IMPORTANCE,
    RetrieveRequest, SEARCH_DEFAULT,
};

/// Over-fetch factor applied to the requested output size so deduplication
/// losses cannot under-fill the block. Two rows per wanted row keeps the fetch
/// bounded (at most twice the promise) while absorbing exact and near
/// duplicates; the result is still clamped to the store's search bounds.
const SEARCH_FETCH_FACTOR: usize = 2;

impl MemoryService {
    /// Retrieves and renders the memory block for `request`.
    ///
    /// At most `request.limit` memories are rendered (`0` uses
    /// [`SEARCH_DEFAULT`], and values above [`MAX_SEARCH_LIMIT`] are clamped to
    /// it). The store fetch is deliberately larger than the output — see
    /// [`SEARCH_FETCH_FACTOR`] — because ranking removes exact and near
    /// duplicates after the fetch. The token budget is a second, independent
    /// bound and may shorten the block further.
    ///
    /// Scopes must name their ids (`("project", Some(prj_…))`); a bare
    /// `("project", None)` is rejected so one project can never read another's
    /// memory by omission. An empty scope list is rejected for the same reason.
    pub async fn retrieve(&self, request: RetrieveRequest) -> Result<MemoryBlock, MemoryError> {
        if request.scopes.is_empty() {
            return Err(MemoryError::invalid(
                "retrieval needs at least one scope: global, project, agent or session",
            ));
        }
        for (scope, scope_id) in &request.scopes {
            validate_scope(scope, scope_id.as_deref())?;
        }

        let want = if request.limit == 0 {
            SEARCH_DEFAULT
        } else {
            request.limit.min(MAX_SEARCH_LIMIT)
        };
        let fetch = want
            .saturating_mul(SEARCH_FETCH_FACTOR)
            .clamp(MIN_SEARCH_LIMIT, MAX_SEARCH_LIMIT)
            .max(want);
        let query = request.query.trim();
        let hits = self
            .store
            .search_memories(MemoryQuery {
                scopes: request.scopes.clone(),
                query: (!query.is_empty()).then(|| query.to_owned()),
                kinds: request.kinds,
                tags: request.tags,
                limit: fetch,
                pinned_min_importance: Some(PINNED_IMPORTANCE),
            })
            .await?;

        let mut ranked = rank_and_dedupe(hits);
        ranked.truncate(want);
        let lines: Vec<(String, String)> = ranked
            .iter()
            .map(|hit| {
                (
                    hit.id.clone(),
                    format!(
                        "- [{}] {} ({})",
                        hit.kind,
                        normalize_line(&hit.text),
                        hit.id
                    ),
                )
            })
            .collect();
        let budget = if request.token_budget == 0 {
            DEFAULT_TOKEN_BUDGET
        } else {
            request.token_budget
        };
        Ok(render_block(block_scope(&request.scopes), &lines, budget))
    }
}
