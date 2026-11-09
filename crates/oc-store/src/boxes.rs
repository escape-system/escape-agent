//! Durable context boxes (`docs/25-context-boxes.md`).
//!
//! Boxes are the stable substrate a compaction summary points at instead of
//! restating. They are addressed by `key` inside one session tree (`root_id`),
//! carry a `stable` or `temporary` lifetime, and are read on demand: the prompt
//! only ever renders the index.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Map, Value, json};

use crate::db::{Store, in_tx, now_ms};
use crate::error::StoreError;

/// Accepted lifetimes. `stable` survives compactions, `temporary` expires on
/// each successful one.
pub const BOX_KINDS: [&str; 2] = ["stable", "temporary"];

/// Hard content cap accepted by the store. A box above this cannot be loaded
/// in a bounded number of reads, so the write fails instead of truncating.
pub const MAX_BOX_CHARS: usize = 262_144;

/// Keys are addressable identifiers: lowercase letters, digits, `.`, `_`, `-`.
pub const MAX_BOX_KEY_CHARS: usize = 64;

const LIST_DEFAULT: usize = 100;
const LIST_CAP: usize = 200;

pub(crate) const BOX_COLUMNS: &str = "id, root_id, owner_session, key, title, description, kind, \
tags, content, revision, time_created, time_updated, time_expired";

/// A box to create or update. Create-or-update is keyed by
/// `(root_id, key)`: an existing box is rewritten and its `revision` bumped,
/// so a retried write never creates a duplicate. A soft-deleted row is
/// recreated in place with a fresh revision history.
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

/// Filters for [`Store::list_boxes`].
#[derive(Debug, Clone, Default)]
pub struct BoxQuery {
    /// Session tree root.
    pub root_id: String,
    /// Restrict to one kind (`stable`/`temporary`).
    pub kind: Option<String>,
    /// Include `temporary` boxes already expired by a compaction.
    pub include_expired: bool,
    /// Page size; `0` uses the default (100) and values above 200 are clamped.
    pub limit: usize,
}

/// One page of box rows plus whether more remain.
#[derive(Debug, Clone)]
pub struct BoxPage {
    /// Wire box rows ordered by `key`.
    pub items: Vec<Value>,
    /// True when the limit cut the result set.
    pub has_more: bool,
}

impl Store {
    /// Creates or rewrites a box inside `new.root_id`.
    ///
    /// `append = true` joins the new text to the existing content with one
    /// newline. The read-check-write happens in the writer transaction, so two
    /// subagents appending concurrently cannot lose a line. Returns the
    /// committed wire row.
    /// Creates or rewrites a box inside `new.root_id`.
    ///
    /// `append = true` joins the new text to the existing content with one
    /// newline. The read-check-write happens in the writer transaction, so two
    /// subagents appending concurrently cannot lose a line. Returns the
    /// committed wire row.
    #[allow(clippy::too_many_lines)] // One transaction: validate, revive or append, then return the committed row.
    pub async fn write_box(&self, new: NewBox, append: bool) -> Result<Value, StoreError> {
        validate_key(&new.key)?;
        if !BOX_KINDS.contains(&new.kind.as_str()) {
            return Err(StoreError::Invalid(format!(
                "unknown box kind {:?}; expected `stable` or `temporary`",
                new.kind
            )));
        }
        if new.content.chars().count() > MAX_BOX_CHARS {
            return Err(StoreError::Invalid(format!(
                "box content exceeds {MAX_BOX_CHARS} characters"
            )));
        }
        let title = clean(new.title);
        let description = clean(new.description);
        let tags = encode_tags(&new.tags)?;
        let content = new.content;
        let id = oc_types::ContextBoxId::new().into_inner();
        let created = now_ms();
        let root = new.root_id;
        let owner = new.owner_session;
        let key = new.key;
        let kind = new.kind;

        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let root_exists: Option<i64> = transaction
                    .query_row("SELECT 1 FROM session_v2 WHERE id = ?1", [&root], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if root_exists.is_none() {
                    return Err(StoreError::NotFound(format!("session {root}")));
                }
                let existing: Option<(String, String, i64, i64)> = transaction
                    .query_row(
                        "SELECT id, content, deleted, time_created FROM context_box
                         WHERE root_id = ?1 AND key = ?2",
                        params![&root, &key],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                if let Some((existing_id, previous, deleted, previous_created)) = existing {
                    let recreated = deleted != 0;
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
                    // A deleted key starts a new history in place: revision 1
                    // and a fresh creation time, so recreation is observable.
                    let revision = if recreated { 1 } else { i64::MIN };
                    let time_created = if recreated { created } else { previous_created };
                    transaction.execute(
                        "UPDATE context_box
                         SET content = ?2, title = coalesce(?3, title),
                             description = coalesce(?4, description),
                             tags = ?5,
                             revision = CASE WHEN ?9 = 1 THEN ?10 ELSE revision + 1 END,
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
                transaction.execute(
                    "INSERT INTO context_box
                        (id, root_id, owner_session, key, title, description, kind, tags,
                         content, revision, time_created, time_updated, time_expired, deleted)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?10, NULL, 0)",
                    params![
                        &id,
                        &root,
                        &owner,
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

    /// One live box by key, or the expired row when `include_expired`.
    pub async fn get_box(
        &self,
        root_id: &str,
        key: &str,
        include_expired: bool,
    ) -> Result<Option<Value>, StoreError> {
        let root_id = root_id.to_owned();
        let key = key.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {BOX_COLUMNS} FROM context_box
                 WHERE root_id = ?1 AND key = ?2 AND deleted = 0
                   {expired}",
                expired = if include_expired {
                    ""
                } else {
                    "AND time_expired IS NULL"
                }
            );
            query_optional(connection, &sql, params![&root_id, &key])
        })
        .await
    }

    /// Boxes of one tree ordered by key. `temporary` rows expired by a
    /// compaction are hidden unless requested.
    pub async fn list_boxes(&self, query: BoxQuery) -> Result<BoxPage, StoreError> {
        let limit = if query.limit == 0 {
            LIST_DEFAULT
        } else {
            query.limit.min(LIST_CAP)
        };
        let mut sql = format!(
            "SELECT {BOX_COLUMNS} FROM context_box
             WHERE root_id = ?1 AND deleted = 0"
        );
        let mut values: Vec<String> = vec![query.root_id];
        if let Some(kind) = query.kind {
            sql.push_str(" AND kind = ?2");
            values.push(kind);
        }
        if !query.include_expired {
            sql.push_str(" AND time_expired IS NULL");
        }
        sql.push_str(" ORDER BY key ASC LIMIT ");
        sql.push_str(&(limit.saturating_add(1)).to_string());

        self.read(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(values.iter()))?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(box_value(row)?);
            }
            let has_more = items.len() > limit;
            items.truncate(limit);
            Ok(BoxPage { items, has_more })
        })
        .await
    }

    /// Soft-deletes a box. Returns whether a live row was changed.
    pub async fn delete_box(&self, root_id: &str, key: &str) -> Result<bool, StoreError> {
        let root_id = root_id.to_owned();
        let key = key.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE context_box SET deleted = 1, time_updated = ?3
                     WHERE root_id = ?1 AND key = ?2 AND deleted = 0",
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
    pub async fn expire_temporary_boxes(&self, root_id: &str) -> Result<usize, StoreError> {
        let root_id = root_id.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE context_box SET time_expired = ?2, time_updated = ?2
                     WHERE root_id = ?1 AND kind = 'temporary' AND deleted = 0
                       AND time_expired IS NULL",
                    params![&root_id, time],
                )?;
                Ok(changed)
            })
        })
        .await
    }

    /// Every live box key of the tree, used to render the index and to decide
    /// whether a compaction changed the box set.
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
    let key: String = row.get(3)?;
    let title: Option<String> = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let kind: String = row.get(6)?;
    let tags: String = row.get(7)?;
    let content: String = row.get(8)?;
    let revision: i64 = row.get(9)?;
    let time_created: i64 = row.get(10)?;
    let time_updated: i64 = row.get(11)?;
    let time_expired: Option<i64> = row.get(12)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("rootID".to_owned(), Value::String(root_id));
    map.insert("ownerSession".to_owned(), Value::String(owner_session));
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
