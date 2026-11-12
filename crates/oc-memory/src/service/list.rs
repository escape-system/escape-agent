//! Offset pagination over the store's capped list query.

use serde_json::Value;

use crate::error::MemoryError;
use oc_store::MemoryQuery;

use super::scope::validate_scope;
use super::{LIST_DEFAULT, LIST_MAX, ListRequest, MemoryService};

/// The store caps `list_memories` at this many rows, so a page can never start
/// past it without returning empty while rows still exist.
pub(super) const LIST_FETCH_CAP: usize = 200;

impl MemoryService {
    /// Lists memories for the requested scopes, most important and most recent
    /// first.
    pub async fn list(&self, request: ListRequest) -> Result<Vec<Value>, MemoryError> {
        if request.scopes.is_empty() {
            return Err(MemoryError::invalid(
                "listing needs at least one scope: global, project, agent or session",
            ));
        }
        for (scope, scope_id) in &request.scopes {
            validate_scope(scope, scope_id.as_deref())?;
        }
        let offset = parse_cursor(request.cursor.as_deref())?;
        let limit = if request.limit == 0 {
            LIST_DEFAULT
        } else {
            request.limit.min(LIST_MAX)
        };
        let fetch = offset.saturating_add(limit).min(LIST_FETCH_CAP);
        if fetch <= offset {
            return Ok(Vec::new());
        }
        let kinds: Vec<String> = request
            .kind
            .into_iter()
            .map(|kind| kind.trim().to_owned())
            .filter(|kind| !kind.is_empty())
            .collect();
        let items = self
            .store
            .list_memories(MemoryQuery {
                scopes: request.scopes,
                query: None,
                kinds,
                tags: request.tags,
                limit: fetch,
                pinned_min_importance: None,
            })
            .await?;
        Ok(items.into_iter().skip(offset).take(limit).collect())
    }
}

fn parse_cursor(cursor: Option<&str>) -> Result<usize, MemoryError> {
    match cursor.map(str::trim) {
        None | Some("") => Ok(0),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| MemoryError::invalid("cursor is not a memory list cursor")),
    }
}
