//! Offset pagination over the store's capped list query.
//!
//! The store returns at most [`LIST_FETCH_CAP`] rows and takes no offset, so a
//! page is the requested slice of one capped fetch. The page reports `more`
//! from the fetch itself instead of letting the caller infer it from
//! `limit + 1` arithmetic, and it reports when it stopped at the cap rather
//! than at the end of the data ([`MemoryPage::window_exhausted`]). A cursor at
//! or past the cap is rejected: the old silent empty page read as "no more
//! memories" while rows still existed.

use serde_json::Value;

use crate::error::MemoryError;
use oc_store::MemoryQuery;

use super::scope::validate_scope;
use super::{LIST_DEFAULT, LIST_MAX, ListRequest, MemoryPage, MemoryService};

/// The store caps `list_memories` at this many rows, so a page can never start
/// past it. The cap is legitimate; pagination must not pretend otherwise.
pub const LIST_FETCH_CAP: usize = 200;

/// One extra row fetched beyond the page so that "another page exists" is
/// observed in the fetch instead of inferred from a full page.
const LOOKAHEAD_ROWS: usize = 1;

impl MemoryService {
    /// Lists one page of memories for the requested scopes, most important and
    /// most recent first.
    ///
    /// `cursor` is a bare decimal offset. The store fetch is
    /// `min(offset + limit + 1, LIST_FETCH_CAP)` rows, so `more` is `true` only
    /// when a row actually followed the page.
    ///
    /// A cursor past the window (`offset >= LIST_FETCH_CAP`) is an explicit
    /// [`MemoryError::Invalid`]: the window cannot serve it, and answering
    /// "empty page" would claim the result set ended when it did not.
    pub async fn list_page(&self, request: ListRequest) -> Result<MemoryPage, MemoryError> {
        if request.scopes.is_empty() {
            return Err(MemoryError::invalid(
                "listing needs at least one scope: global, project, agent or session",
            ));
        }
        for (scope, scope_id) in &request.scopes {
            validate_scope(scope, scope_id.as_deref())?;
        }
        let offset = parse_cursor(request.cursor.as_deref())?;
        if offset >= LIST_FETCH_CAP {
            return Err(MemoryError::invalid(format!(
                "cursor {offset} is past the {LIST_FETCH_CAP}-memory pagination window; \
                 start again from cursor 0 and narrow the scope or filters"
            )));
        }
        let limit = if request.limit == 0 {
            LIST_DEFAULT
        } else {
            request.limit.min(LIST_MAX)
        };
        let fetch = offset
            .saturating_add(limit)
            .saturating_add(LOOKAHEAD_ROWS)
            .min(LIST_FETCH_CAP);
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

        let fetched = items.len();
        let items: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
        let more = fetched > offset.saturating_add(items.len());
        // The store cannot report a total, so a page that ends exactly at the
        // cap is *possibly* followed by more rows: say the window ended instead
        // of claiming the data ended.
        let window_exhausted =
            offset.saturating_add(items.len()) >= LIST_FETCH_CAP && fetched == fetch;
        Ok(MemoryPage {
            items,
            more,
            window_exhausted,
        })
    }

    /// Lists memories for the requested scopes, most important and most recent
    /// first, without pagination metadata.
    ///
    /// Convenience wrapper over [`MemoryService::list_page`] for callers that
    /// read one page and ignore whether more rows exist.
    pub async fn list(&self, request: ListRequest) -> Result<Vec<Value>, MemoryError> {
        Ok(self.list_page(request).await?.items)
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
