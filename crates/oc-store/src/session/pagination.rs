//! Keyset cursor payloads, filter merging and page-cursor construction.
//!
//! The anchor is `(time_updated, id)`; the direction says whether the page walks
//! away from the start of the list or back towards it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cursor::{self, Cursor, Direction, Order};
use crate::db::{DEFAULT_LIMIT, MAX_LIMIT};
use crate::error::StoreError;

use super::SessionQuery;

/// Cursor payload for sessions: the `(time_updated, id)` anchor plus the filters.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SessionCursor {
    id: String,
    time: i64,
    order: Order,
    direction: Direction,
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

/// Decoded anchor plus the direction, order and filters of one list request.
pub(super) struct Anchor {
    pub(super) position: Option<(i64, String)>,
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
    let (position, direction, order, filters) = match query.cursor.take() {
        Some(raw) => {
            let decoded: SessionCursor = cursor::decode(&raw)?;
            let filters = decoded.filters.merge(query)?;
            (
                Some((decoded.time, decoded.id)),
                decoded.direction,
                decoded.order,
                filters,
            )
        }
        None => (
            None,
            Direction::Next,
            query.order,
            CursorFilters::from_query(query),
        ),
    };
    Ok(Anchor {
        position,
        direction,
        order,
        filters,
    })
}

/// Builds the neighbouring-page cursors for one already-truncated page.
pub(super) fn page_cursor(
    rows: &[(i64, String, Value)],
    direction: Direction,
    order: Order,
    anchor: Option<&(i64, String)>,
    has_more: bool,
    filters: &CursorFilters,
) -> Result<Cursor, StoreError> {
    let encode = |time: i64, id: &str, direction: Direction| {
        cursor::encode(&SessionCursor {
            id: id.to_owned(),
            time,
            order,
            direction,
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
