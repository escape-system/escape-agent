//! Durable context boxes (`docs/25-context-boxes.md`).
//!
//! Boxes are the stable substrate a compaction summary points at instead of
//! restating. They are addressed by `key` inside one session tree (`root_id`),
//! carry a `stable` or `temporary` lifetime, and are read on demand: the prompt
//! only ever renders the index.

use std::collections::BTreeSet;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::{Map, Value, json};

use crate::db::{Store, in_tx, now_ms};
use crate::error::StoreError;

/// Accepted lifetimes. `stable` survives compactions, `temporary` expires on
/// each successful one.
pub const BOX_KINDS: [&str; 2] = ["stable", "temporary"];

/// Accepted visibility scopes within one session tree.
/// `tree` boxes are shared across all sessions under `root_id`;
/// `session` boxes are private to `owner_session`.
pub const BOX_SCOPES: [&str; 2] = ["tree", "session"];

/// Hard content cap accepted by the store. A box above this cannot be loaded
/// in a bounded number of reads, so the write fails instead of truncating.
pub const MAX_BOX_CHARS: usize = 262_144;

/// Keys are addressable identifiers: lowercase letters, digits, `.`, `_`, `-`.
pub const MAX_BOX_KEY_CHARS: usize = 64;

const LIST_DEFAULT: usize = 100;
const LIST_CAP: usize = 200;
const SEARCH_DEFAULT: usize = 20;
const SEARCH_CAP: usize = 50;

/// Owner ids bound into one `IN (...)` list. SQLite's default host-parameter
/// limit is 999; 500 leaves headroom for the fixed parameters and keeps the
/// statement small.
const OWNER_CHUNK: usize = 500;

pub(crate) const BOX_COLUMNS: &str = "id, root_id, owner_session, scope, key, title, description, \
kind, tags, content, revision, time_created, time_updated, time_expired";

/// A box to create or update in the shared tree scope. Create-or-update is
/// keyed by `(root_id, scope='tree', key)`: an existing box is rewritten and
/// its `revision` bumped, so a retried write never creates a duplicate. A
/// soft-deleted row is recreated in place with a fresh revision history.
#[derive(Debug, Clone)]
pub struct NewBox {
    /// Owning session tree root (`ses_…`).
    pub root_id: String,
    /// Session that writes the row (the caller, not the root).
    pub owner_session: String,
    /// Stable address inside the tree.
    pub key: String,
    /// Display title; falls back to the key in the index.
    pub title: Option<String>,
    /// One-line description rendered in the index.
    pub description: Option<String>,
    /// `stable` or `temporary`.
    pub kind: String,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// Full content after any append.
    pub content: String,
}

/// Scoped box write with optional preservation of existing `kind`/`tags` on
/// update or append, plus optional optimistic concurrency (`expected_revision`).
#[derive(Debug, Clone, Default)]
pub struct WriteBox {
    /// Owning session tree root (`ses_…`).
    pub root_id: String,
    /// Session that writes the row (and owns `session`-scoped rows).
    pub owner_session: String,
    /// `tree` (default) or `session`.
    pub scope: Option<String>,
    /// Box key (`[a-z0-9._-]{1,64}`).
    pub key: String,
    /// Display title; keeps existing value on live update when omitted,
    /// cleared when recreating a deleted key.
    pub title: Option<String>,
    /// One-line description rendered in the index.
    pub description: Option<String>,
    /// `stable` or `temporary`. On live updates/appends, `None` preserves the
    /// existing box kind; on new creates it defaults to `stable`.
    pub kind: Option<String>,
    /// Free-form tags. On live updates/appends, `None` preserves existing tags.
    pub tags: Option<Vec<String>>,
    /// Content to write or append.
    pub content: String,
    /// Optional optimistic lock: fails if live revision != expected.
    pub expected_revision: Option<i64>,
}

/// Targeted find-and-replace inside an existing live, unexpired box.
#[derive(Debug, Clone)]
pub struct EditBox {
    /// Owning session tree root.
    pub root_id: String,
    /// Session performing the edit.
    pub owner_session: String,
    /// Explicit `tree` / `session`, or `None` to prefer caller `session` then `tree`.
    pub scope: Option<String>,
    /// Box key.
    pub key: String,
    /// Exact non-empty text to match.
    pub find: String,
    /// Replacement text.
    pub replace: String,
    /// Replace every match instead of requiring a unique match.
    pub replace_all: bool,
    /// Optional optimistic lock against current revision.
    pub expected_revision: Option<i64>,
}

/// Metadata/lifecycle/scope update without rewriting box content.
#[derive(Debug, Clone, Default)]
pub struct BoxMetaPatch {
    /// Owning session tree root.
    pub root_id: String,
    /// Calling session id.
    pub owner_session: String,
    /// Current scope filter (`tree`/`session`) or `None` for session-first fallback.
    pub scope: Option<String>,
    /// Current box key.
    pub key: String,
    /// Optional renamed key.
    pub new_key: Option<String>,
    /// Optional target scope (`tree` <-> `session`).
    pub new_scope: Option<String>,
    /// Optional target kind (`stable` <-> `temporary`).
    pub kind: Option<String>,
    /// Optional new title.
    pub title: Option<String>,
    /// Optional new description.
    pub description: Option<String>,
    /// Optional replacement tags.
    pub tags: Option<Vec<String>>,
    /// Clear `time_expired` without rewriting content.
    pub revive: bool,
    /// Optional optimistic lock.
    pub expected_revision: Option<i64>,
}

/// Filters for [`Store::list_boxes`].
#[derive(Debug, Clone, Default)]
pub struct BoxQuery {
    /// Session tree root.
    pub root_id: String,
    /// Restrict to one scope (`tree`/`session`).
    pub scope: Option<String>,
    /// When set, `session` boxes belong only to this `owner_session`.
    pub session_id: Option<String>,
    /// Restrict to one kind (`stable`/`temporary`).
    pub kind: Option<String>,
    /// Key prefix filter.
    pub prefix: Option<String>,
    /// Exact tag filter.
    pub tag: Option<String>,
    /// Pagination cursor returned by a previous `BoxPage::next_cursor`.
    pub cursor: Option<String>,
    /// Include `temporary` boxes already expired by a compaction.
    pub include_expired: bool,
    /// Page size; `0` uses the default (100) and values above 200 are clamped.
    pub limit: usize,
}

/// Filters for [`Store::search_boxes`].
#[derive(Debug, Clone, Default)]
pub struct BoxSearchQuery {
    /// Session tree root.
    pub root_id: String,
    /// Restrict to one scope (`tree`/`session`).
    pub scope: Option<String>,
    /// Caller session id (isolates `session` scope boxes).
    pub session_id: Option<String>,
    /// Restrict to one kind (`stable`/`temporary`).
    pub kind: Option<String>,
    /// Key prefix filter.
    pub prefix: Option<String>,
    /// Exact tag filter.
    pub tag: Option<String>,
    /// Include expired temporary boxes.
    pub include_expired: bool,
    /// Case-insensitive substring query.
    pub query: String,
    /// Maximum hits (default 20, cap 50).
    pub limit: usize,
}

/// One page of box rows plus whether more remain.
#[derive(Debug, Clone)]
pub struct BoxPage {
    /// Wire box rows ordered by `key`.
    pub items: Vec<Value>,
    /// True when the limit cut the result set.
    pub has_more: bool,
    /// Opaque pagination cursor when `has_more` is true.
    pub next_cursor: Option<String>,
}

impl Store {
    /// Creates or rewrites a tree-scoped box inside `new.root_id`.
    ///
    /// `append = true` joins the new text to the existing content with one
    /// newline. The read-check-write happens in the writer transaction, so two
    /// subagents appending concurrently cannot lose a line. Returns the
    /// committed wire row.
    pub async fn write_box(&self, new: NewBox, append: bool) -> Result<Value, StoreError> {
        self.write_box_scoped(
            WriteBox {
                root_id: new.root_id,
                owner_session: new.owner_session,
                scope: Some("tree".to_owned()),
                key: new.key,
                title: new.title,
                description: new.description,
                kind: Some(new.kind),
                tags: Some(new.tags),
                content: new.content,
                expected_revision: None,
            },
            append,
        )
        .await
    }

    /// Creates or updates a `tree` or `session`-scoped box with optional kind/tag
    /// preservation and optimistic revision locking.
    #[allow(clippy::too_many_lines)] // One transaction: validate, check revision, revive/append, return committed row.
    pub async fn write_box_scoped(&self, new: WriteBox, append: bool) -> Result<Value, StoreError> {
        validate_key(&new.key)?;
        let scope = validate_scope(new.scope.as_deref())?;
        let requested_kind = match new.kind {
            Some(kind) => {
                validate_kind(&kind)?;
                Some(kind)
            }
            None => None,
        };
        if new.content.chars().count() > MAX_BOX_CHARS {
            return Err(StoreError::Invalid(format!(
                "box content exceeds {MAX_BOX_CHARS} characters"
            )));
        }
        let title = clean(new.title);
        let description = clean(new.description);
        let requested_tags = match new.tags {
            Some(tags) => Some(encode_tags(&tags)?),
            None => None,
        };
        let content = new.content;
        let id = oc_types::ContextBoxId::new().into_inner();
        let created = now_ms();
        let root = new.root_id;
        let owner = new.owner_session;
        let key = new.key;
        let expected_revision = new.expected_revision;

        self.write(move |connection| {
            type ExistingRow = (String, String, i64, i64, String, String, i64);
            in_tx(connection, |transaction| {
                let root_exists: Option<i64> = transaction
                    .query_row("SELECT 1 FROM session_v2 WHERE id = ?1", [&root], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if root_exists.is_none() {
                    return Err(StoreError::NotFound(format!("session {root}")));
                }
                let existing: Option<ExistingRow> = if scope == "session" {
                    transaction
                        .query_row(
                            "SELECT id, content, deleted, time_created, kind, tags, revision
                             FROM context_box
                             WHERE root_id = ?1 AND scope = 'session' AND owner_session = ?2 AND key = ?3",
                            params![&root, &owner, &key],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                    row.get(5)?,
                                    row.get(6)?,
                                ))
                            },
                        )
                        .optional()?
                } else {
                    transaction
                        .query_row(
                            "SELECT id, content, deleted, time_created, kind, tags, revision
                             FROM context_box
                             WHERE root_id = ?1 AND scope = 'tree' AND key = ?2",
                            params![&root, &key],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                    row.get(5)?,
                                    row.get(6)?,
                                ))
                            },
                        )
                        .optional()?
                };

                if let Some((
                    existing_id,
                    previous,
                    deleted,
                    previous_created,
                    existing_kind,
                    existing_tags,
                    current_rev,
                )) = existing
                {
                    let recreated = deleted != 0;
                    if let Some(expected) = expected_revision {
                        if recreated {
                            return Err(StoreError::Invalid(format!(
                                "revision conflict on box `{key}`: box was deleted (expected revision {expected})"
                            )));
                        }
                        if current_rev != expected {
                            return Err(StoreError::Invalid(format!(
                                "revision conflict on box `{key}`: expected revision {expected}, actual {current_rev}"
                            )));
                        }
                    }
                    let next = if append && !recreated {
                        if previous.is_empty() {
                            content
                        } else if content.is_empty() {
                            previous
                        } else {
                            format!("{previous}\n{content}")
                        }
                    } else {
                        content
                    };
                    if next.chars().count() > MAX_BOX_CHARS {
                        return Err(StoreError::Invalid(format!(
                            "box content exceeds {MAX_BOX_CHARS} characters"
                        )));
                    }
                    let kind = match &requested_kind {
                        Some(kind) => kind.clone(),
                        None if !recreated => existing_kind,
                        None => "stable".to_owned(),
                    };
                    let tags = match &requested_tags {
                        Some(tags) => tags.clone(),
                        None if !recreated => existing_tags,
                        None => "[]".to_owned(),
                    };
                    // A deleted key starts a new history in place: revision 1,
                    // fresh creation time, and cleared title/description when omitted.
                    let revision = if recreated { 1 } else { current_rev + 1 };
                    let time_created = if recreated {
                        created
                    } else {
                        previous_created
                    };
                    transaction.execute(
                        "UPDATE context_box
                         SET content = ?2,
                             title = CASE WHEN ?9 = 1 THEN ?3 ELSE coalesce(?3, title) END,
                             description = CASE WHEN ?9 = 1 THEN ?4 ELSE coalesce(?4, description) END,
                             tags = ?5,
                             revision = ?10,
                             time_created = ?11, time_updated = ?6,
                             owner_session = ?7, deleted = 0, kind = ?8,
                             time_expired = NULL
                         WHERE id = ?1",
                        params![
                            &existing_id,
                            &next,
                            title,
                            description,
                            &tags,
                            created,
                            &owner,
                            &kind,
                            i64::from(recreated),
                            revision,
                            time_created,
                        ],
                    )?;
                    return query_box(transaction, &existing_id);
                }

                if let Some(expected) = expected_revision {
                    return Err(StoreError::Invalid(format!(
                        "revision conflict on box `{key}`: box does not exist yet (expected revision {expected})"
                    )));
                }
                let kind = requested_kind.unwrap_or_else(|| "stable".to_owned());
                let tags = requested_tags.unwrap_or_else(|| "[]".to_owned());
                transaction.execute(
                    "INSERT INTO context_box
                        (id, root_id, owner_session, scope, key, title, description, kind, tags,
                         content, revision, time_created, time_updated, time_expired, deleted)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?11, NULL, 0)",
                    params![
                        &id,
                        &root,
                        &owner,
                        &scope,
                        &key,
                        title,
                        description,
                        &kind,
                        &tags,
                        &content,
                        created,
                    ],
                )?;
                query_box(transaction, &id)
            })
        })
        .await
    }

    /// Targeted find-and-replace on a live, non-expired box.
    pub async fn edit_box(&self, edit: EditBox) -> Result<Value, StoreError> {
        validate_key(&edit.key)?;
        if edit.find.is_empty() {
            return Err(StoreError::Invalid("`find` must not be empty".to_owned()));
        }
        let scope = edit
            .scope
            .as_deref()
            .map(|value| validate_scope(Some(value)))
            .transpose()?;
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(target) = resolve_live_box_for_write(
                    transaction,
                    &edit.root_id,
                    scope.as_deref(),
                    &edit.owner_session,
                    &edit.key,
                )?
                else {
                    return Err(StoreError::NotFound(format!(
                        "box `{}` in root {}",
                        edit.key, edit.root_id
                    )));
                };
                if target.time_expired.is_some() {
                    return Err(StoreError::Invalid(format!(
                        "box `{}` was expired by a compaction; revive it with box_meta or box_write first",
                        edit.key
                    )));
                }
                if let Some(expected) = edit.expected_revision
                    && target.revision != expected
                {
                    return Err(StoreError::Invalid(format!(
                        "revision conflict on box `{}`: expected revision {expected}, actual {}",
                        edit.key, target.revision
                    )));
                }
                let matches = target.content.match_indices(&edit.find).count();
                if matches == 0 {
                    return Err(StoreError::Invalid(format!(
                        "text to replace was not found in box `{}`",
                        edit.key
                    )));
                }
                if matches > 1 && !edit.replace_all {
                    return Err(StoreError::Invalid(format!(
                        "text matched {matches} times in box `{}`; provide more context or set replace_all=true",
                        edit.key
                    )));
                }
                let next = if edit.replace_all {
                    target.content.replace(&edit.find, &edit.replace)
                } else {
                    target.content.replacen(&edit.find, &edit.replace, 1)
                };
                if next.trim().is_empty() {
                    return Err(StoreError::Invalid(
                        "edit would make box empty; call box_delete instead".to_owned(),
                    ));
                }
                if next.chars().count() > MAX_BOX_CHARS {
                    return Err(StoreError::Invalid(format!(
                        "box content exceeds {MAX_BOX_CHARS} characters"
                    )));
                }
                transaction.execute(
                    "UPDATE context_box
                     SET content = ?2, revision = revision + 1, time_updated = ?3,
                         owner_session = ?4
                     WHERE id = ?1",
                    params![&target.id, &next, time, &edit.owner_session],
                )?;
                query_box(transaction, &target.id)
            })
        })
        .await
    }

    /// Update metadata, lifetime kind, scope, or key name without rewriting content.
    #[allow(clippy::too_many_lines)] // Single atomic transaction checking conflicts and reviving/promoting.
    pub async fn update_box_meta(&self, patch: BoxMetaPatch) -> Result<Value, StoreError> {
        validate_key(&patch.key)?;
        if let Some(new_key) = patch.new_key.as_deref() {
            validate_key(new_key)?;
        }
        let scope = patch
            .scope
            .as_deref()
            .map(|value| validate_scope(Some(value)))
            .transpose()?;
        let new_scope = patch
            .new_scope
            .as_deref()
            .map(|value| validate_scope(Some(value)))
            .transpose()?;
        if let Some(kind) = patch.kind.as_deref() {
            validate_kind(kind)?;
        }
        let title = clean(patch.title);
        let description = clean(patch.description);
        let tags = match patch.tags {
            Some(tags) => Some(encode_tags(&tags)?),
            None => None,
        };
        if patch.new_key.is_none()
            && new_scope.is_none()
            && patch.kind.is_none()
            && title.is_none()
            && description.is_none()
            && tags.is_none()
            && !patch.revive
        {
            return Err(StoreError::Invalid(
                "box_meta requires at least one change (kind, scope, new_key, title, description, tags, or revive)".to_owned(),
            ));
        }

        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(target) = resolve_live_box_for_write(
                    transaction,
                    &patch.root_id,
                    scope.as_deref(),
                    &patch.owner_session,
                    &patch.key,
                )?
                else {
                    return Err(StoreError::NotFound(format!(
                        "box `{}` in root {}",
                        patch.key, patch.root_id
                    )));
                };
                if target.time_expired.is_some() && !patch.revive {
                    return Err(StoreError::Invalid(format!(
                        "box `{}` is expired; pass revive=true to restore it",
                        patch.key
                    )));
                }
                if let Some(expected) = patch.expected_revision
                    && target.revision != expected
                {
                    return Err(StoreError::Invalid(format!(
                        "revision conflict on box `{}`: expected revision {expected}, actual {}",
                        patch.key, target.revision
                    )));
                }

                let target_scope = new_scope.unwrap_or_else(|| target.scope.clone());
                let target_key = patch.new_key.clone().unwrap_or_else(|| target.key.clone());
                let next_owner = if target_scope == "session" && target.scope == "session" {
                    target.owner_session.clone()
                } else {
                    patch.owner_session.clone()
                };

                if target_scope != target.scope || target_key != target.key {
                    let collision: Option<(String, i64)> = if target_scope == "session" {
                        transaction
                            .query_row(
                                "SELECT id, deleted FROM context_box
                                 WHERE root_id = ?1 AND scope = 'session'
                                   AND owner_session = ?2 AND key = ?3 AND id != ?4",
                                params![&patch.root_id, &next_owner, &target_key, &target.id],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()?
                    } else {
                        transaction
                            .query_row(
                                "SELECT id, deleted FROM context_box
                                 WHERE root_id = ?1 AND scope = 'tree'
                                   AND key = ?2 AND id != ?3",
                                params![&patch.root_id, &target_key, &target.id],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()?
                    };
                    if let Some((other_id, deleted)) = collision {
                        if deleted == 0 {
                            return Err(StoreError::Invalid(format!(
                                "target box `{target_key}` already exists in scope `{target_scope}`"
                            )));
                        }
                        // Drop soft-deleted tombstone so partial unique index stays clean.
                        transaction
                            .execute("DELETE FROM context_box WHERE id = ?1", [&other_id])?;
                    }
                }

                transaction.execute(
                    "UPDATE context_box
                     SET scope = ?2,
                         key = ?3,
                         owner_session = ?4,
                         kind = coalesce(?5, kind),
                         title = coalesce(?6, title),
                         description = coalesce(?7, description),
                         tags = coalesce(?8, tags),
                         time_expired = CASE WHEN ?9 = 1 THEN NULL ELSE time_expired END,
                         revision = revision + 1,
                         time_updated = ?10
                     WHERE id = ?1",
                    params![
                        &target.id,
                        &target_scope,
                        &target_key,
                        &next_owner,
                        patch.kind,
                        title,
                        description,
                        tags,
                        i64::from(patch.revive),
                        time,
                    ],
                )?;
                query_box(transaction, &target.id)
            })
        })
        .await
    }

    /// One live `tree` box by key, or the expired row when `include_expired`.
    pub async fn get_box(
        &self,
        root_id: &str,
        key: &str,
        include_expired: bool,
    ) -> Result<Option<Value>, StoreError> {
        self.get_box_scoped(root_id, Some("tree"), None, key, include_expired)
            .await
    }

    /// One live box resolved by scope (`tree` or `session`). When `scope` is
    /// `None` and `session_id` is given, caller `session` scope is checked
    /// before shared `tree` scope.
    pub async fn get_box_scoped(
        &self,
        root_id: &str,
        scope: Option<&str>,
        session_id: Option<&str>,
        key: &str,
        include_expired: bool,
    ) -> Result<Option<Value>, StoreError> {
        let scope = scope.map(|value| validate_scope(Some(value))).transpose()?;
        let root_id = root_id.to_owned();
        let session_id = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let key = key.to_owned();
        let expired = if include_expired {
            ""
        } else {
            "AND time_expired IS NULL"
        };

        self.read(move |connection| {
            let check_session =
                scope.as_deref() == Some("session") || (scope.is_none() && session_id.is_some());
            if check_session && let Some(sid) = session_id.as_deref() {
                let sql = format!(
                    "SELECT {BOX_COLUMNS} FROM context_box
                     WHERE root_id = ?1 AND scope = 'session' AND owner_session = ?2
                       AND key = ?3 AND deleted = 0 {expired}"
                );
                if let Some(found) = query_optional(connection, &sql, params![&root_id, sid, &key])?
                {
                    return Ok(Some(found));
                }
            }
            if scope.as_deref() == Some("session") {
                return Ok(None);
            }
            let sql = format!(
                "SELECT {BOX_COLUMNS} FROM context_box
                 WHERE root_id = ?1 AND scope = 'tree' AND key = ?2 AND deleted = 0 {expired}"
            );
            query_optional(connection, &sql, params![&root_id, &key])
        })
        .await
    }

    /// Boxes of one tree ordered by key. `temporary` rows expired by a
    /// compaction are hidden unless requested.
    pub async fn list_boxes(&self, query: BoxQuery) -> Result<BoxPage, StoreError> {
        let scope = query
            .scope
            .as_deref()
            .map(|value| validate_scope(Some(value)))
            .transpose()?;
        if let Some(kind) = query.kind.as_deref() {
            validate_kind(kind)?;
        }
        let cursor = match clean(query.cursor) {
            Some(raw) => Some(parse_cursor(&raw)?),
            None => None,
        };
        let limit = if query.limit == 0 {
            LIST_DEFAULT
        } else {
            query.limit.min(LIST_CAP)
        };
        let mut sql = format!(
            "SELECT {BOX_COLUMNS} FROM context_box
             WHERE root_id = ? AND deleted = 0"
        );
        let mut values: Vec<SqlValue> = vec![SqlValue::Text(query.root_id)];
        append_visibility_filter(
            &mut sql,
            &mut values,
            scope.as_deref(),
            query.session_id.as_deref(),
        );
        if let Some(kind) = query.kind {
            sql.push_str(" AND kind = ?");
            values.push(SqlValue::Text(kind));
        }
        if !query.include_expired {
            sql.push_str(" AND time_expired IS NULL");
        }
        if let Some(prefix) = clean(query.prefix) {
            sql.push_str(" AND key LIKE ? ESCAPE '\\'");
            values.push(SqlValue::Text(format!("{}%", escape_like(&prefix))));
        }
        if let Some(tag) = clean(query.tag) {
            sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(context_box.tags) WHERE value = ?)");
            values.push(SqlValue::Text(tag));
        }
        if let Some((cursor_key, cursor_id)) = cursor {
            sql.push_str(" AND (key > ? OR (key = ? AND id > ?))");
            values.push(SqlValue::Text(cursor_key.clone()));
            values.push(SqlValue::Text(cursor_key));
            values.push(SqlValue::Text(cursor_id));
        }
        sql.push_str(" ORDER BY key ASC, id ASC LIMIT ?");
        values.push(SqlValue::Integer(
            i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX),
        ));

        self.read(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(box_value(row)?);
            }
            let has_more = items.len() > limit;
            items.truncate(limit);
            let next_cursor = if has_more {
                items.last().and_then(|item| {
                    let key = item.get("key")?.as_str()?;
                    let id = item.get("id")?.as_str()?;
                    Some(format!("{key}:{id}"))
                })
            } else {
                None
            };
            Ok(BoxPage {
                items,
                has_more,
                next_cursor,
            })
        })
        .await
    }

    /// Search boxes by case-insensitive substring across key, metadata, tags,
    /// and content. Returns full box rows annotated with `matchIn`,
    /// `matchCount`, and optional `line`, `charOffset`, and `snippet`.
    pub async fn search_boxes(&self, query: BoxSearchQuery) -> Result<Vec<Value>, StoreError> {
        let needle = query.query.trim().to_owned();
        if needle.is_empty() {
            return Err(StoreError::Invalid("`query` must not be empty".to_owned()));
        }
        let scope = query
            .scope
            .as_deref()
            .map(|value| validate_scope(Some(value)))
            .transpose()?;
        if let Some(kind) = query.kind.as_deref() {
            validate_kind(kind)?;
        }
        let limit = if query.limit == 0 {
            SEARCH_DEFAULT
        } else {
            query.limit.min(SEARCH_CAP)
        };
        let mut sql = format!(
            "SELECT {BOX_COLUMNS} FROM context_box
             WHERE root_id = ? AND deleted = 0"
        );
        let mut values: Vec<SqlValue> = vec![SqlValue::Text(query.root_id)];
        append_visibility_filter(
            &mut sql,
            &mut values,
            scope.as_deref(),
            query.session_id.as_deref(),
        );
        if let Some(kind) = query.kind {
            sql.push_str(" AND kind = ?");
            values.push(SqlValue::Text(kind));
        }
        if !query.include_expired {
            sql.push_str(" AND time_expired IS NULL");
        }
        if let Some(prefix) = clean(query.prefix) {
            sql.push_str(" AND key LIKE ? ESCAPE '\\'");
            values.push(SqlValue::Text(format!("{}%", escape_like(&prefix))));
        }
        if let Some(tag) = clean(query.tag) {
            sql.push_str(" AND EXISTS (SELECT 1 FROM json_each(context_box.tags) WHERE value = ?)");
            values.push(SqlValue::Text(tag));
        }
        sql.push_str(" ORDER BY time_updated DESC, key ASC");

        let needle_lower = needle.to_lowercase();
        self.read(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut hits = Vec::new();
            while let Some(row) = rows.next()? {
                let item = box_value(row)?;
                if let Some(hit) = score_box_match(item, &needle_lower) {
                    hits.push(hit);
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
            Ok(hits)
        })
        .await
    }

    /// Soft-deletes a `tree`-scoped box. Returns whether a live row was changed.
    pub async fn delete_box(&self, root_id: &str, key: &str) -> Result<bool, StoreError> {
        self.delete_box_scoped(root_id, Some("tree"), None, key)
            .await
    }

    /// Soft-deletes a box in `scope`, preferring caller `session` scope before
    /// shared `tree` scope when `scope` is `None`.
    pub async fn delete_box_scoped(
        &self,
        root_id: &str,
        scope: Option<&str>,
        session_id: Option<&str>,
        key: &str,
    ) -> Result<bool, StoreError> {
        let scope = scope.map(|value| validate_scope(Some(value))).transpose()?;
        let root_id = root_id.to_owned();
        let session_id = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let key = key.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let check_session = scope.as_deref() == Some("session")
                    || (scope.is_none() && session_id.is_some());
                if check_session && let Some(sid) = session_id.as_deref() {
                    let changed = transaction.execute(
                        "UPDATE context_box SET deleted = 1, time_updated = ?4
                         WHERE root_id = ?1 AND scope = 'session' AND owner_session = ?2
                           AND key = ?3 AND deleted = 0",
                        params![&root_id, sid, &key, time],
                    )?;
                    if changed > 0 {
                        return Ok(true);
                    }
                }
                if scope.as_deref() == Some("session") {
                    return Ok(false);
                }
                let changed = transaction.execute(
                    "UPDATE context_box SET deleted = 1, time_updated = ?3
                     WHERE root_id = ?1 AND scope = 'tree' AND key = ?2 AND deleted = 0",
                    params![&root_id, &key, time],
                )?;
                Ok(changed > 0)
            })
        })
        .await
    }

    /// Expires every live `temporary` box of the tree. Called once per
    /// successful compaction, after the `completed` row commits.
    ///
    /// Returns the number of boxes expired. Stable boxes are never touched;
    /// an already expired row keeps its first expiry time.
    ///
    /// This is the whole-tree form of [`Self::expire_temporary_boxes_for`]; it
    /// expires rows regardless of which session of the tree wrote them.
    pub async fn expire_temporary_boxes(&self, root_id: &str) -> Result<usize, StoreError> {
        self.expire_temporary_boxes_scoped(root_id.to_owned(), None)
            .await
    }

    /// Expires the live `temporary` boxes of `root_id` written by `owners`.
    ///
    /// A compaction is scoped to the compacting session and its descendants,
    /// so a sibling subtree's temporary boxes must survive it (R3). An empty
    /// `owners` expires nothing: the caller asked for a subtree, and an empty
    /// subtree is not the whole tree.
    ///
    /// Owner ids are bound in chunks of [`OWNER_CHUNK`] to stay under SQLite's
    /// host-parameter limit. All chunks run in one transaction, so the expiry
    /// is atomic and idempotent: rows already expired keep their first
    /// `time_expired`.
    pub async fn expire_temporary_boxes_for(
        &self,
        root_id: &str,
        owners: &[String],
    ) -> Result<usize, StoreError> {
        let owners: Vec<String> = owners
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if owners.is_empty() {
            return Ok(0);
        }
        self.expire_temporary_boxes_scoped(root_id.to_owned(), Some(owners))
            .await
    }

    /// Shared expiry write. `None` owners means the whole tree; `Some` means
    /// only rows whose `owner_session` is in that non-empty list.
    async fn expire_temporary_boxes_scoped(
        &self,
        root_id: String,
        owners: Option<Vec<String>>,
    ) -> Result<usize, StoreError> {
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(owners) = owners else {
                    let changed = transaction.execute(
                        "UPDATE context_box SET time_expired = ?2, time_updated = ?2
                         WHERE root_id = ?1 AND kind = 'temporary' AND deleted = 0
                           AND time_expired IS NULL",
                        params![&root_id, time],
                    )?;
                    return Ok(changed);
                };
                let mut changed = 0usize;
                for chunk in owners.chunks(OWNER_CHUNK) {
                    let placeholders = vec!["?"; chunk.len()].join(", ");
                    let sql = format!(
                        "UPDATE context_box SET time_expired = ?2, time_updated = ?2
                         WHERE root_id = ?1 AND kind = 'temporary' AND deleted = 0
                           AND time_expired IS NULL
                           AND owner_session IN ({placeholders})"
                    );
                    let mut values: Vec<SqlValue> = Vec::with_capacity(chunk.len() + 2);
                    values.push(SqlValue::Text(root_id.clone()));
                    values.push(SqlValue::Integer(time));
                    values.extend(chunk.iter().map(|owner| SqlValue::Text(owner.clone())));
                    changed += transaction.execute(&sql, params_from_iter(values.iter()))?;
                }
                Ok(changed)
            })
        })
        .await
    }

    /// Every distinct live box key of the tree, used for index probes and
    /// tests.
    pub async fn box_keys(&self, root_id: &str) -> Result<BTreeSet<String>, StoreError> {
        let root_id = root_id.to_owned();
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT key FROM context_box
                 WHERE root_id = ?1 AND deleted = 0 AND time_expired IS NULL
                 ORDER BY key ASC",
            )?;
            let mut rows = statement.query([&root_id])?;
            let mut keys = BTreeSet::new();
            while let Some(row) = rows.next()? {
                keys.insert(row.get::<_, String>(0)?);
            }
            Ok(keys)
        })
        .await
    }
}

struct LiveBoxTarget {
    id: String,
    scope: String,
    owner_session: String,
    key: String,
    content: String,
    revision: i64,
    time_expired: Option<i64>,
}

fn resolve_live_box_for_write(
    transaction: &rusqlite::Transaction<'_>,
    root_id: &str,
    scope: Option<&str>,
    owner_session: &str,
    key: &str,
) -> Result<Option<LiveBoxTarget>, StoreError> {
    let parse_row = |row: &Row<'_>| {
        Ok(LiveBoxTarget {
            id: row.get(0)?,
            scope: row.get(1)?,
            owner_session: row.get(2)?,
            key: row.get(3)?,
            content: row.get(4)?,
            revision: row.get(5)?,
            time_expired: row.get(6)?,
        })
    };
    if scope == Some("session") || scope.is_none() {
        let found = transaction
            .query_row(
                "SELECT id, scope, owner_session, key, content, revision, time_expired
                 FROM context_box
                 WHERE root_id = ?1 AND scope = 'session' AND owner_session = ?2
                   AND key = ?3 AND deleted = 0",
                params![root_id, owner_session, key],
                parse_row,
            )
            .optional()?;
        if found.is_some() || scope == Some("session") {
            return Ok(found);
        }
    }
    transaction
        .query_row(
            "SELECT id, scope, owner_session, key, content, revision, time_expired
             FROM context_box
             WHERE root_id = ?1 AND scope = 'tree' AND key = ?2 AND deleted = 0",
            params![root_id, key],
            parse_row,
        )
        .optional()
        .map_err(Into::into)
}

fn append_visibility_filter(
    sql: &mut String,
    values: &mut Vec<SqlValue>,
    scope: Option<&str>,
    session_id: Option<&str>,
) {
    let sid = session_id.map(str::trim).filter(|value| !value.is_empty());
    match (scope, sid) {
        (None, Some(sid)) => {
            sql.push_str(" AND (scope = 'tree' OR (scope = 'session' AND owner_session = ?))");
            values.push(SqlValue::Text(sid.to_owned()));
        }
        (Some("tree"), _) => {
            sql.push_str(" AND scope = 'tree'");
        }
        (Some("session"), Some(sid)) => {
            sql.push_str(" AND scope = 'session' AND owner_session = ?");
            values.push(SqlValue::Text(sid.to_owned()));
        }
        (Some("session"), None) => {
            sql.push_str(" AND scope = 'session'");
        }
        (None, None) | (Some(_), _) => {}
    }
}

fn validate_kind(kind: &str) -> Result<(), StoreError> {
    if !BOX_KINDS.contains(&kind) {
        return Err(StoreError::Invalid(format!(
            "unknown box kind {kind:?}; expected `stable` or `temporary`"
        )));
    }
    Ok(())
}

fn validate_scope(scope: Option<&str>) -> Result<String, StoreError> {
    let scope = scope
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("tree");
    if !BOX_SCOPES.contains(&scope) {
        return Err(StoreError::Invalid(format!(
            "unknown box scope {scope:?}; expected `tree` or `session`"
        )));
    }
    Ok(scope.to_owned())
}

fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn parse_cursor(raw: &str) -> Result<(String, String), StoreError> {
    let Some((key, id)) = raw.split_once(':') else {
        return Err(StoreError::Invalid(
            "invalid box pagination cursor".to_owned(),
        ));
    };
    if key.is_empty() || id.is_empty() {
        return Err(StoreError::Invalid(
            "invalid box pagination cursor".to_owned(),
        ));
    }
    Ok((key.to_owned(), id.to_owned()))
}

fn score_box_match(mut item: Value, needle_lower: &str) -> Option<Value> {
    let content = item
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let key = item
        .get("key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let description = item
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tags = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    let (match_in, match_count, first_char_pos) =
        if let Some((count, pos)) = find_case_insensitive_matches(&content, needle_lower) {
            ("content", count, Some(pos))
        } else if key.to_lowercase().contains(needle_lower) {
            ("key", 1, None)
        } else if title.to_lowercase().contains(needle_lower) {
            ("title", 1, None)
        } else if description.to_lowercase().contains(needle_lower) {
            ("description", 1, None)
        } else if tags.to_lowercase().contains(needle_lower) {
            ("tags", 1, None)
        } else {
            return None;
        };

    let map = item.as_object_mut()?;
    map.insert("matchIn".to_owned(), Value::String(match_in.to_owned()));
    map.insert("matchCount".to_owned(), json!(match_count));
    if let Some(char_offset) = first_char_pos {
        let prefix: String = content.chars().take(char_offset).collect();
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let snippet_start = char_offset.saturating_sub(60);
        let snippet: String = content.chars().skip(snippet_start).take(220).collect();
        let clean_snippet = snippet.split_whitespace().collect::<Vec<_>>().join(" ");
        map.insert("charOffset".to_owned(), json!(char_offset));
        map.insert("line".to_owned(), json!(line));
        map.insert("snippet".to_owned(), Value::String(clean_snippet));
    }
    Some(item)
}

fn find_case_insensitive_matches(haystack: &str, needle_lower: &str) -> Option<(usize, usize)> {
    let needle_chars: Vec<char> = needle_lower.chars().collect();
    if needle_chars.is_empty() {
        return None;
    }
    let mut hay_lower_chars: Vec<(usize, char)> = Vec::new();
    for (char_idx, ch) in haystack.chars().enumerate() {
        for lower in ch.to_lowercase() {
            hay_lower_chars.push((char_idx, lower));
        }
    }
    if hay_lower_chars.len() < needle_chars.len() {
        return None;
    }
    let mut count = 0usize;
    let mut first_pos = None;
    let mut idx = 0usize;
    while idx + needle_chars.len() <= hay_lower_chars.len() {
        let matched = needle_chars
            .iter()
            .enumerate()
            .all(|(offset, target)| hay_lower_chars[idx + offset].1 == *target);
        if matched {
            if first_pos.is_none() {
                first_pos = Some(hay_lower_chars[idx].0);
            }
            count += 1;
            idx += needle_chars.len();
        } else {
            idx += 1;
        }
    }
    first_pos.map(|pos| (count, pos))
}

fn query_optional(
    connection: &Connection,
    sql: &str,
    values: impl rusqlite::Params,
) -> Result<Option<Value>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(values)?;
    match rows.next()? {
        Some(row) => Ok(Some(box_value(row)?)),
        None => Ok(None),
    }
}

fn query_box(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<Value, StoreError> {
    let sql = format!("SELECT {BOX_COLUMNS} FROM context_box WHERE id = ?1");
    query_optional(transaction, &sql, [id])?
        .ok_or_else(|| StoreError::NotFound(format!("box {id}")))
}

fn box_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let root_id: String = row.get(1)?;
    let owner_session: String = row.get(2)?;
    let scope: String = row.get(3)?;
    let key: String = row.get(4)?;
    let title: Option<String> = row.get(5)?;
    let description: Option<String> = row.get(6)?;
    let kind: String = row.get(7)?;
    let tags: String = row.get(8)?;
    let content: String = row.get(9)?;
    let revision: i64 = row.get(10)?;
    let time_created: i64 = row.get(11)?;
    let time_updated: i64 = row.get(12)?;
    let time_expired: Option<i64> = row.get(13)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("rootID".to_owned(), Value::String(root_id));
    map.insert("ownerSession".to_owned(), Value::String(owner_session));
    map.insert("scope".to_owned(), Value::String(scope));
    map.insert("key".to_owned(), Value::String(key));
    if let Some(title) = title {
        map.insert("title".to_owned(), Value::String(title));
    }
    if let Some(description) = description {
        map.insert("description".to_owned(), Value::String(description));
    }
    map.insert("kind".to_owned(), Value::String(kind));
    map.insert(
        "tags".to_owned(),
        Value::Array(decode_tags(&tags)?.into_iter().map(Value::String).collect()),
    );
    map.insert("content".to_owned(), Value::String(content.clone()));
    map.insert("size".to_owned(), json!(content.chars().count()));
    map.insert("revision".to_owned(), json!(revision));
    map.insert("timeCreated".to_owned(), json!(time_created));
    map.insert("timeUpdated".to_owned(), json!(time_updated));
    if let Some(time_expired) = time_expired {
        map.insert("timeExpired".to_owned(), json!(time_expired));
    }
    Ok(Value::Object(map))
}

fn validate_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty() || key.chars().count() > MAX_BOX_KEY_CHARS {
        return Err(StoreError::Invalid(format!(
            "box key must be 1..={MAX_BOX_KEY_CHARS} characters"
        )));
    }
    if !key.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '.' | '_' | '-')
    }) {
        return Err(StoreError::Invalid(
            "box key may only contain lowercase letters, digits, `.`, `_` and `-`".to_owned(),
        ));
    }
    Ok(())
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn encode_tags(tags: &[String]) -> Result<String, StoreError> {
    let mut seen = BTreeSet::new();
    let mut cleaned = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() || !seen.insert(tag.to_owned()) {
            continue;
        }
        cleaned.push(tag.to_owned());
        if cleaned.len() >= 32 {
            break;
        }
    }
    Ok(serde_json::to_string(&cleaned)?)
}

fn decode_tags(raw: &str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str(raw)
        .map_err(|error| StoreError::Corrupt(format!("box tags are invalid: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_restricted_to_addressable_characters() {
        assert!(validate_key("decisions").is_ok());
        assert!(validate_key("a.b_c-d1").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("Upper").is_err());
        assert!(validate_key("with space").is_err());
        assert!(validate_key(&"a".repeat(MAX_BOX_KEY_CHARS + 1)).is_err());
    }
}
