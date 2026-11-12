//! Retrieval: validate the visible scopes, search, rank and render a block.

use crate::error::MemoryError;
use crate::render::{DEFAULT_TOKEN_BUDGET, normalize_line, render_block};
use oc_store::MemoryQuery;

use super::rank::rank_and_dedupe;
use super::scope::{block_scope, validate_scope};
use super::{
    MAX_SEARCH_LIMIT, MIN_SEARCH_LIMIT, MemoryBlock, MemoryService, PINNED_IMPORTANCE,
    RetrieveRequest,
};

impl MemoryService {
    /// Retrieves and renders the memory block for `request`.
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

        let limit = request.limit.clamp(MIN_SEARCH_LIMIT, MAX_SEARCH_LIMIT);
        let query = request.query.trim();
        let hits = self
            .store
            .search_memories(MemoryQuery {
                scopes: request.scopes.clone(),
                query: (!query.is_empty()).then(|| query.to_owned()),
                kinds: request.kinds,
                tags: request.tags,
                limit,
                pinned_min_importance: Some(PINNED_IMPORTANCE),
            })
            .await?;

        let ranked = rank_and_dedupe(hits);
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
