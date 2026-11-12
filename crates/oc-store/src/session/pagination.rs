//! Keyset cursor payloads, filter merging and page-cursor construction.
//!
//! The anchor is `(time_updated, id)`; the direction says whether the page walks
//! away from the start of the list or back towards it.
//!
//! # Stable walks
//!
//! `time_updated` is mutable: every appended message, inbox item or event runs
//! [`crate::db::touch_session`], and `session/patch.rs` bumps it directly. A
//! plain keyset walk over a mutable key is therefore not stable, because a row
//! that moves while the walk is in progress can move relative to the cursor.
//!
//! A walk is frozen at its first page. That page records
//! `snapshot = max(time_updated)` over the rows matching its filters — read by
//! the same statement (and therefore the same read transaction) as the page
//! itself — and every later page of that walk adds `time_updated <= snapshot`.
//! The contract for a walk:
//!
//! - the eligible set is the sessions matching the walk's filters whose
//!   `time_updated` was at most `snapshot` when the walk started;
//! - a session created after the first page is not part of the walk, and neither
//!   is one whose `time_updated` grows above `snapshot`; start a new walk to see
//!   them. This is intended, not a limitation: a walk is a consistent view of the
//!   session list as of its first page, and both kinds of row are on the first
//!   page of the next walk;
//! - each eligible key is visited at most once, so the ascending-direction and
//!   backward (`Previous`) walks no longer repeat a session that was touched
//!   after it had already been read;
//! - **not** guaranteed: a row touched to a time that is still inside the
//!   snapshot but on the far side of the cursor moves *within* the walk, so a
//!   not-yet-visited row can still be missed. Exactly-once over a mutable sort key
//!   is impossible for a stateless cursor: it needs an immutable key
//!   (`rowid`/`time_created` as the `ORDER BY`) or a materialized result set.
//!   Both are schema/UX decisions that this module does not make; the bound is the
//!   part that needs neither a schema revision nor a column reordering;
//! - the bound is only as good as the clock. A caller that writes a *smaller*
//!   `time_updated` (a wall-clock regression through `patch_session`) can still
//!   move a row across the cursor. `touch_session` itself only raises the value.
//!
//! Cursors encoded before the `snapshot` field existed decode with `None` and keep
//! the previous unbounded behaviour for the rest of that walk, so nothing already
//! persisted or in flight breaks. The field is additive in both directions: an
//! older binary ignores the unknown key, and a `None` bound is not serialized, so
//! a legacy walk keeps emitting legacy cursors.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cursor::{self, Cursor, Direction, Order};
use crate::db::{DEFAULT_LIMIT, MAX_LIMIT};
use crate::error::StoreError;

use super::SessionQuery;

/// Cursor payload for sessions: the `(time_updated, id)` anchor plus the filters.
///
/// `snapshot` freezes the walk: it is `max(time_updated)` as read by the first
/// page, and later pages bound themselves by it. It is omitted when absent, so a
/// walk started by a cursor that predates the field keeps the legacy unbounded
/// behaviour instead of being silently re-frozen at a different point.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SessionCursor {
    id: String,
    time: i64,
    order: Order,
    direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot: Option<i64>,
    #[serde(default)]
    filters: CursorFilters,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct CursorFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[allow(clippy::option_option)] // None = no filter, Some(None) = roots only
    pub(super) parent: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) subpath: Option<String>,
}

impl CursorFilters {
    fn from_query(query: &SessionQuery) -> Self {
        Self {
            search: query.search.clone(),
            parent: query.parent_id.clone(),
            archived: query.archived,
            directory: query.directory.clone(),
            project: query.project.clone(),
            subpath: query.subpath.clone(),
        }
    }

    fn merge(self, query: &SessionQuery) -> Result<Self, StoreError> {
        Ok(Self {
            search: merge_filter(self.search, query.search.clone(), "search")?,
            parent: merge_filter(self.parent, query.parent_id.clone(), "parentID")?,
            archived: merge_filter(self.archived, query.archived, "archived")?,
            directory: merge_filter(self.directory, query.directory.clone(), "directory")?,
            project: merge_filter(self.project, query.project.clone(), "project")?,
            subpath: merge_filter(self.subpath, query.subpath.clone(), "subpath")?,
        })
    }
}

fn merge_filter<T: PartialEq>(
    stored: Option<T>,
    requested: Option<T>,
    name: &str,
) -> Result<Option<T>, StoreError> {
    match (stored, requested) {
        (Some(stored), Some(requested)) if stored != requested => Err(StoreError::InvalidCursor(
            format!("cursor {name} filter does not match the query"),
        )),
        (Some(stored), _) => Ok(Some(stored)),
        (None, requested) => Ok(requested),
    }
}

/// Decoded anchor plus the direction, order, snapshot bound and filters of one
/// list request.
pub(super) struct Anchor {
    /// `(time_updated, id)` of the cursor row; `None` on the first page.
    pub(super) position: Option<(i64, String)>,
    /// Upper bound on `time_updated` for this walk; `None` for a legacy cursor
    /// that predates the field (unbounded, the pre-freeze behaviour).
    pub(super) snapshot: Option<i64>,
    pub(super) direction: Direction,
    pub(super) order: Order,
    pub(super) filters: CursorFilters,
}

impl Anchor {
    /// SQL order actually used: a `Previous` page is read in the opposite
    /// direction and reversed afterwards.
    pub(super) fn sql_order(&self) -> Order {
        if self.direction == Direction::Previous {
            self.order.flipped()
        } else {
            self.order
        }
    }
}

/// Normalizes `query.limit` and decodes `query.cursor` into an [`Anchor`].
pub(super) fn resolve(query: &mut SessionQuery) -> Result<Anchor, StoreError> {
    query.limit = if query.limit == 0 {
        DEFAULT_LIMIT
    } else {
        query.limit.min(MAX_LIMIT)
    };
    let (position, snapshot, direction, order, filters) = match query.cursor.take() {
        Some(raw) => {
            let decoded: SessionCursor = cursor::decode(&raw)?;
            let filters = decoded.filters.merge(query)?;
            (
                Some((decoded.time, decoded.id)),
                decoded.snapshot,
                decoded.direction,
                decoded.order,
                filters,
            )
        }
        None => (
            None,
            None,
            Direction::Next,
            query.order,
            CursorFilters::from_query(query),
        ),
    };
    Ok(Anchor {
        position,
        snapshot,
        direction,
        order,
        filters,
    })
}

/// Builds the neighbouring-page cursors for one already-truncated page.
///
/// `snapshot` is the walk's frozen bound: the first page passes the value it
/// computed, every later page passes the one it decoded, and `None` (a legacy
/// cursor) keeps producing legacy cursors without the field.
pub(super) fn page_cursor(
    rows: &[(i64, String, Value)],
    direction: Direction,
    order: Order,
    anchor: Option<&(i64, String)>,
    snapshot: Option<i64>,
    has_more: bool,
    filters: &CursorFilters,
) -> Result<Cursor, StoreError> {
    let encode = |time: i64, id: &str, direction: Direction| {
        cursor::encode(&SessionCursor {
            id: id.to_owned(),
            time,
            order,
            direction,
            snapshot,
            filters: filters.clone(),
        })
    };
    let mut page_cursor = Cursor::default();
    if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
        let next = match direction {
            Direction::Next => {
                if has_more {
                    Some(encode(last.0, &last.1, Direction::Next)?)
                } else {
                    None
                }
            }
            Direction::Previous => Some(encode(last.0, &last.1, Direction::Next)?),
        };
        let previous = match direction {
            Direction::Next => {
                if anchor.is_some() {
                    Some(encode(first.0, &first.1, Direction::Previous)?)
                } else {
                    None
                }
            }
            Direction::Previous => {
                if has_more {
                    Some(encode(first.0, &first.1, Direction::Previous)?)
                } else {
                    None
                }
            }
        };
        page_cursor = Cursor { previous, next };
    }
    Ok(page_cursor)
}
