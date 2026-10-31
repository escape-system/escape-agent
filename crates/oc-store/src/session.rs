//! Session rows, patching and keyset pagination over `session_v2`.

use std::fmt::Write as _;

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::cursor::{self, Cursor, Direction, Order};
use crate::db::{self, DEFAULT_LIMIT, MAX_LIMIT, Store, in_tx, now_ms};
use crate::error::StoreError;
use crate::project::ensure_project;

/// Input for creating a session.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// Wire session id (`ses_…`).
    pub id: String,
    /// Parent session for a subagent session.
    pub parent_id: Option<String>,
    /// Owning project id (`prj_…`).
    pub project_id: String,
    /// Absolute working directory.
    pub directory: String,
    /// Display title.
    pub title: Option<String>,
    /// Selected agent id.
    pub agent: Option<String>,
    /// Selected model (`Session.Info` model object).
    pub model: Option<Value>,
    /// Host-supplied metadata object.
    pub metadata: Option<Value>,
    /// Permission ruleset.
    pub permissions: Option<Value>,
    /// Creation time in epoch milliseconds.
    pub time_created: i64,
}

/// Explicit tree/policy columns for [`Store::create_session_with_tree`].
#[derive(Debug, Clone)]
pub struct NewSessionTree {
    /// Root session of the tree the new session belongs to.
    pub root_id: String,
    /// Distance from the root; must be the parent's depth plus one.
    pub depth: u32,
    /// `primary`, `subagent` or `coordinator`.
    pub role: String,
    /// Whether the new session may spawn children.
    pub spawn_allowed: bool,
}

/// Filters and pagination for [`Store::list_sessions`].
#[derive(Debug, Clone)]
pub struct SessionQuery {
    /// Page size; `0` means the default (50) and values above 200 are clamped.
    pub limit: usize,
    /// Sort order on `(time_updated, id)`.
    pub order: Order,
    /// Case-insensitive substring match on the title (documented table scan).
    pub search: Option<String>,
    /// `Some(None)` selects root sessions, `Some(Some(id))` selects children.
    pub parent_id: Option<Option<String>>,
    /// `Some(true)` selects archived sessions, `Some(false)` unarchived ones.
    pub archived: Option<bool>,
    /// Exact working-directory match.
    pub directory: Option<String>,
    /// Exact project match.
    pub project: Option<String>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
}

impl Default for SessionQuery {
    fn default() -> Self {
        Self {
            limit: DEFAULT_LIMIT,
            order: Order::Desc,
            search: None,
            parent_id: None,
            archived: None,
            directory: None,
            project: None,
            cursor: None,
        }
    }
}

/// Fields to change on a session. Absent fields are left untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionPatch {
    /// New title.
    pub title: Option<String>,
    /// New selected agent.
    pub agent: Option<String>,
    /// New selected model; JSON `null` clears the column.
    pub model: Option<Value>,
    /// New metadata object; JSON `null` clears the column.
    pub metadata: Option<Value>,
    /// New permission ruleset; JSON `null` clears the column.
    pub permissions: Option<Value>,
    /// New working directory.
    pub directory: Option<String>,
    /// New subpath.
    pub path: Option<String>,
    /// New revert state; JSON `null` clears the column.
    pub revert: Option<Value>,
    /// Archive or unarchive the session.
    pub archived: Option<bool>,
}

/// One page of sessions.
#[derive(Debug, Clone)]
pub struct SessionPage {
    /// Wire `Session.Info` objects in the requested order.
    pub items: Vec<Value>,
    /// Keyset cursors for the neighbouring pages.
    pub cursor: Cursor,
}

pub(crate) const SESSION_COLUMNS: &str = "id, project_id, parent_id, root_id, depth, role, \
spawn_allowed, child_count, fork_session_id, fork_boundary, directory, path, title, agent, \
metadata, permission, model, revert, summary_diffs, cost, tokens_input, tokens_output, \
tokens_reasoning, tokens_cache_read, tokens_cache_write, time_created, time_updated, time_busy, \
time_idle, time_viewed, idle_outcome, time_compacting, time_archived, time_suspended, \
resume_attempts";

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
struct CursorFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[allow(clippy::option_option)] // None = no filter, Some(None) = roots only
    parent: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

impl CursorFilters {
    fn from_query(query: &SessionQuery) -> Self {
        Self {
            search: query.search.clone(),
            parent: query.parent_id.clone(),
            archived: query.archived,
            directory: query.directory.clone(),
            project: query.project.clone(),
        }
    }

    fn merge(self, query: &SessionQuery) -> Result<Self, StoreError> {
        Ok(Self {
            search: merge_filter(self.search, query.search.clone(), "search")?,
            parent: merge_filter(self.parent, query.parent_id.clone(), "parentID")?,
            archived: merge_filter(self.archived, query.archived, "archived")?,
            directory: merge_filter(self.directory, query.directory.clone(), "directory")?,
            project: merge_filter(self.project, query.project.clone(), "project")?,
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

impl Store {
    /// Creates a session and returns its wire `Session.Info`.
    ///
    /// The owning project row is inserted when missing so the foreign key holds.
    /// `root_id`/`depth` are derived from the parent and the policy columns keep
    /// their defaults (`primary`, spawning allowed).
    pub async fn create_session(&self, new: NewSession) -> Result<Value, StoreError> {
        self.create_session_inner(new, None).await
    }

    /// Creates a session with explicit tree columns in one transaction,
    /// incrementing the parent's `child_count` when `parent_id` is set.
    ///
    /// `tree.role` must be `primary`, `subagent` or `coordinator`;
    /// `tree.root_id` must not be empty; and when a parent is given,
    /// `tree.depth` must equal the parent's depth + 1. Duplicate ids are a
    /// `Conflict` and leave the parent's count untouched.
    pub async fn create_session_with_tree(
        &self,
        new: NewSession,
        tree: NewSessionTree,
    ) -> Result<Value, StoreError> {
        validate_tree(&tree)?;
        self.create_session_inner(new, Some(tree)).await
    }

    /// Updates the policy columns of an existing session.
    ///
    /// Unknown ids are a `NotFound`; unknown roles are `Invalid`.
    pub async fn set_session_policy(
        &self,
        id: &str,
        role: &str,
        spawn_allowed: bool,
    ) -> Result<(), StoreError> {
        validate_role(role)?;
        let id = id.to_owned();
        let role = role.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE session_v2 SET role = ?2, spawn_allowed = ?3 WHERE id = ?1",
                    params![&id, &role, i64::from(spawn_allowed)],
                )?;
                if changed == 0 {
                    return Err(StoreError::NotFound(format!("session {id}")));
                }
                Ok(())
            })
        })
        .await
    }

    async fn create_session_inner(
        &self,
        new: NewSession,
        tree: Option<NewSessionTree>,
    ) -> Result<Value, StoreError> {
        let NewSession {
            id,
            parent_id,
            project_id,
            directory,
            title,
            agent,
            model,
            metadata,
            permissions,
            time_created,
        } = new;
        let id_for_read = id.clone();
        let parent_for_count = parent_id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let existing: Option<i64> = transaction
                    .query_row("SELECT 1 FROM session_v2 WHERE id = ?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if existing.is_some() {
                    return Err(StoreError::Conflict(format!("session {id} already exists")));
                }
                ensure_project(transaction, &project_id, &directory, time_created)?;
                let (root_id, depth, role, spawn_allowed) = if let Some(tree) = tree {
                    if let Some(parent) = parent_id.as_deref() {
                        let Some((_, parent_depth)) = parent_position(transaction, parent)? else {
                            return Err(StoreError::NotFound(format!("parent session {parent}")));
                        };
                        if i64::from(tree.depth) != parent_depth + 1 {
                            return Err(StoreError::Invalid(format!(
                                "depth {} does not match parent depth {}",
                                tree.depth, parent_depth
                            )));
                        }
                    }
                    (
                        tree.root_id,
                        i64::from(tree.depth),
                        tree.role,
                        tree.spawn_allowed,
                    )
                } else {
                    let (root_id, depth) =
                        resolve_root_depth(transaction, &id, parent_id.as_deref(), true)?;
                    (root_id, depth, "primary".to_owned(), true)
                };
                let model = db::encode_json_opt(model.as_ref())?;
                let metadata = db::encode_json_opt(metadata.as_ref())?;
                let permissions = db::encode_json_opt(permissions.as_ref())?;
                transaction.execute(
                    "INSERT INTO session_v2
                        (id, project_id, parent_id, root_id, depth, role, spawn_allowed,
                         directory, title, agent, model, metadata, permission,
                         time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                    params![
                        &id,
                        &project_id,
                        parent_id.as_deref(),
                        &root_id,
                        depth,
                        &role,
                        i64::from(spawn_allowed),
                        &directory,
                        title.as_deref(),
                        agent.as_deref(),
                        model.as_deref(),
                        metadata.as_deref(),
                        permissions.as_deref(),
                        time_created,
                    ],
                )?;
                if let Some(parent) = parent_for_count.as_deref() {
                    transaction.execute(
                        "UPDATE session_v2 SET child_count = child_count + 1 WHERE id = ?1",
                        [parent],
                    )?;
                }
                Ok(())
            })
        })
        .await?;
        self.get_session(&id_for_read)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("session {id_for_read}")))
    }

    /// Loads a session as a wire `Session.Info`.
    pub async fn get_session(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2 WHERE id = ?1");
            let row = connection
                .query_row(&sql, [&id], |row| Ok(session_value(row)))
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Returns the count of active (currently busy or running) children of `parent_id`.
    pub async fn active_child_count(&self, parent_id: &str) -> Result<u32, StoreError> {
        let parent_id = parent_id.to_owned();
        self.read(move |connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM session_v2 
                     WHERE parent_id = ?1 
                       AND (time_idle IS NULL OR (time_busy IS NOT NULL AND time_busy > time_idle))",
                    [&parent_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        })
        .await
    }

    /// Lists sessions with keyset pagination.
    #[allow(clippy::too_many_lines)] // one linear keyset query; splitting hides the cursor contract
    pub async fn list_sessions(&self, query: SessionQuery) -> Result<SessionPage, StoreError> {
        let mut query = query;
        query.limit = if query.limit == 0 {
            DEFAULT_LIMIT
        } else {
            query.limit.min(MAX_LIMIT)
        };
        let (anchor, direction, order, filters) = match query.cursor.take() {
            Some(raw) => {
                let decoded: SessionCursor = cursor::decode(&raw)?;
                let filters = decoded.filters.merge(&query)?;
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
                CursorFilters::from_query(&query),
            ),
        };
        let effective = if direction == Direction::Previous {
            order.flipped()
        } else {
            order
        };

        let mut conditions: Vec<String> = Vec::new();
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(search) = &filters.search {
            conditions.push("title LIKE ?".to_owned());
            values.push(SqlValue::Text(format!("%{search}%")));
        }
        match &filters.parent {
            Some(Some(parent)) => {
                conditions.push("parent_id = ?".to_owned());
                values.push(SqlValue::Text(parent.clone()));
            }
            Some(None) => conditions.push("parent_id IS NULL".to_owned()),
            None => {}
        }
        if let Some(archived) = filters.archived {
            let condition = if archived {
                "time_archived IS NOT NULL"
            } else {
                "time_archived IS NULL"
            };
            conditions.push(condition.to_owned());
        }
        if let Some(directory) = &filters.directory {
            conditions.push("directory = ?".to_owned());
            values.push(SqlValue::Text(directory.clone()));
        }
        if let Some(project) = &filters.project {
            conditions.push("project_id = ?".to_owned());
            values.push(SqlValue::Text(project.clone()));
        }
        if let Some((time, id)) = &anchor {
            let comparator = if effective == Order::Asc { ">" } else { "<" };
            conditions.push(format!(
                "(time_updated {comparator} ? OR (time_updated = ? AND id {comparator} ?))"
            ));
            values.push(SqlValue::Integer(*time));
            values.push(SqlValue::Integer(*time));
            values.push(SqlValue::Text(id.clone()));
        }

        let mut sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2");
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        write!(
            sql,
            " ORDER BY time_updated {}, id {} LIMIT ?",
            effective.sql(),
            effective.sql()
        )
        .expect("writing to a String cannot fail");
        values.push(SqlValue::Integer(
            i64::try_from(query.limit + 1).unwrap_or(i64::MAX),
        ));

        let rows = self
            .read(move |connection| {
                let mut statement = connection.prepare(&sql)?;
                let mut mapped = statement.query(params_from_iter(values.iter()))?;
                let mut items: Vec<(i64, String, Value)> = Vec::new();
                while let Some(row) = mapped.next()? {
                    let id: String = row.get(0)?;
                    let time: i64 = row.get(26)?;
                    items.push((time, id, session_value(row)?));
                }
                Ok(items)
            })
            .await?;

        let has_more = rows.len() > query.limit;
        let mut rows = rows;
        rows.truncate(query.limit);
        if direction == Direction::Previous {
            rows.reverse();
        }

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

        Ok(SessionPage {
            items: rows.into_iter().map(|(_, _, value)| value).collect(),
            cursor: page_cursor,
        })
    }

    /// Applies a patch to a session. Unknown session ids are a `NotFound` error.
    pub async fn patch_session(&self, id: &str, patch: SessionPatch) -> Result<(), StoreError> {
        let id = id.to_owned();
        let updated = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let mut assignments: Vec<&'static str> = Vec::new();
                let mut values: Vec<SqlValue> = Vec::new();
                if let Some(title) = &patch.title {
                    assignments.push("title = ?");
                    values.push(SqlValue::Text(title.clone()));
                }
                if let Some(agent) = &patch.agent {
                    assignments.push("agent = ?");
                    values.push(SqlValue::Text(agent.clone()));
                }
                if let Some(model) = &patch.model {
                    assignments.push("model = ?");
                    values.push(json_sql(Some(model))?);
                }
                if let Some(metadata) = &patch.metadata {
                    assignments.push("metadata = ?");
                    values.push(json_sql(Some(metadata))?);
                }
                if let Some(permissions) = &patch.permissions {
                    assignments.push("permission = ?");
                    values.push(json_sql(Some(permissions))?);
                }
                if let Some(directory) = &patch.directory {
                    assignments.push("directory = ?");
                    values.push(SqlValue::Text(directory.clone()));
                }
                if let Some(path) = &patch.path {
                    assignments.push("path = ?");
                    values.push(SqlValue::Text(path.clone()));
                }
                if let Some(revert) = &patch.revert {
                    assignments.push("revert = ?");
                    values.push(json_sql(Some(revert))?);
                }
                if let Some(archived) = patch.archived {
                    assignments.push("time_archived = ?");
                    values.push(if archived {
                        SqlValue::Integer(updated)
                    } else {
                        SqlValue::Null
                    });
                }
                assignments.push("time_updated = ?");
                values.push(SqlValue::Integer(updated));
                values.push(SqlValue::Text(id.clone()));

                let sql = format!(
                    "UPDATE session_v2 SET {} WHERE id = ?",
                    assignments.join(", ")
                );
                if transaction.execute(&sql, params_from_iter(values.iter()))? == 0 {
                    return Err(StoreError::NotFound(format!("session {id}")));
                }
                Ok(())
            })
        })
        .await
    }

    /// Records the start of an active execution on `session_v2`.
    pub async fn mark_session_busy(&self, id: &str, time_busy: i64) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            connection.execute(
                "UPDATE session_v2
                 SET time_busy = max(?1, coalesce(time_idle + 1, ?1)),
                     idle_outcome = NULL,
                     time_updated = max(time_updated, ?1)
                 WHERE id = ?2",
                params![time_busy, id],
            )?;
            Ok(())
        })
        .await
    }

    /// Records the completion of an execution on `session_v2`.
    pub async fn mark_session_idle(
        &self,
        id: &str,
        time_idle: i64,
        outcome: &str,
    ) -> Result<(), StoreError> {
        let id = id.to_owned();
        let outcome = outcome.to_owned();
        self.write(move |connection| {
            connection.execute(
                "UPDATE session_v2
                 SET time_idle = max(?1, coalesce(time_busy, ?1)),
                     idle_outcome = ?2,
                     time_updated = max(time_updated, ?1)
                 WHERE id = ?3",
                params![time_idle, outcome, id],
            )?;
            Ok(())
        })
        .await
    }

    /// Accumulates step token and cost usage onto `session_v2`.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_session_usage(
        &self,
        id: &str,
        cost: f64,
        tokens_input: i64,
        tokens_output: i64,
        tokens_reasoning: i64,
        tokens_cache_read: i64,
        tokens_cache_write: i64,
    ) -> Result<(), StoreError> {
        let id = id.to_owned();
        let updated = now_ms();
        self.write(move |connection| {
            connection.execute(
                "UPDATE session_v2
                 SET cost = cost + ?1,
                     tokens_input = tokens_input + ?2,
                     tokens_output = tokens_output + ?3,
                     tokens_reasoning = tokens_reasoning + ?4,
                     tokens_cache_read = tokens_cache_read + ?5,
                     tokens_cache_write = tokens_cache_write + ?6,
                     time_updated = max(time_updated, ?7)
                 WHERE id = ?8",
                params![
                    cost,
                    tokens_input,
                    tokens_output,
                    tokens_reasoning,
                    tokens_cache_read,
                    tokens_cache_write,
                    updated,
                    id,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Deletes a session, all of its descendants and their event streams.
    ///
    /// Child rows (messages, parts, inbox items, instructions) are removed by
    /// foreign-key cascade inside the same transaction.
    pub async fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let deleted = delete_tree(transaction, &id)?;
                for aggregate in &deleted {
                    transaction.execute(
                        "DELETE FROM event_sequence WHERE aggregate_id = ?1",
                        [aggregate],
                    )?;
                }
                Ok(())
            })
        })
        .await
    }
}

const ROLES: [&str; 3] = ["primary", "subagent", "coordinator"];

fn validate_role(role: &str) -> Result<(), StoreError> {
    if ROLES.contains(&role) {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "unknown session role {role:?}"
        )))
    }
}

fn validate_tree(tree: &NewSessionTree) -> Result<(), StoreError> {
    if tree.root_id.trim().is_empty() {
        return Err(StoreError::Invalid(
            "tree root_id must not be empty".to_owned(),
        ));
    }
    validate_role(&tree.role)
}

fn session_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let project_id: String = row.get(1)?;
    let parent_id: Option<String> = row.get(2)?;
    let fork_session_id: Option<String> = row.get(8)?;
    let fork_boundary: Option<String> = row.get(9)?;
    let directory: Option<String> = row.get(10)?;
    let path: Option<String> = row.get(11)?;
    let title: Option<String> = row.get(12)?;
    let agent: Option<String> = row.get(13)?;
    let metadata: Option<String> = row.get(14)?;
    let permission: Option<String> = row.get(15)?;
    let model: Option<String> = row.get(16)?;
    let revert: Option<String> = row.get(17)?;
    let cost: f64 = row.get(19)?;
    let tokens_input: i64 = row.get(20)?;
    let tokens_output: i64 = row.get(21)?;
    let tokens_reasoning: i64 = row.get(22)?;
    let tokens_cache_read: i64 = row.get(23)?;
    let tokens_cache_write: i64 = row.get(24)?;
    let time_created: i64 = row.get(25)?;
    let time_updated: i64 = row.get(26)?;
    let time_busy: Option<i64> = row.get(27)?;
    let time_idle: Option<i64> = row.get(28)?;
    let time_viewed: Option<i64> = row.get(29)?;
    let idle_outcome: Option<String> = row.get(30)?;
    let time_archived: Option<i64> = row.get(32)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("projectID".to_owned(), Value::String(project_id));
    if let Some(title) = title {
        map.insert("title".to_owned(), Value::String(title));
    }
    if let Some(parent_id) = parent_id {
        map.insert("parentID".to_owned(), Value::String(parent_id));
    }
    if let (Some(session_id), Some(boundary)) = (fork_session_id, fork_boundary) {
        map.insert(
            "fork".to_owned(),
            json!({
                "sessionID": session_id,
                "boundary": db::decode_json(&boundary)?,
            }),
        );
    }
    if let Some(agent) = agent {
        map.insert("agent".to_owned(), Value::String(agent));
    }
    if let Some(model) = model {
        map.insert("model".to_owned(), db::decode_json(&model)?);
    }
    map.insert("cost".to_owned(), json!(cost));
    map.insert(
        "tokens".to_owned(),
        json!({
            "input": tokens_input,
            "output": tokens_output,
            "reasoning": tokens_reasoning,
            "cache": { "read": tokens_cache_read, "write": tokens_cache_write },
        }),
    );
    if let Some(directory) = directory {
        map.insert("location".to_owned(), json!({ "directory": directory }));
    }
    if let Some(path) = path {
        map.insert("subpath".to_owned(), Value::String(path));
    }
    if let Some(metadata) = metadata {
        map.insert("metadata".to_owned(), db::decode_json(&metadata)?);
    }
    if let Some(permission) = permission {
        map.insert("permissions".to_owned(), db::decode_json(&permission)?);
    }
    if let Some(revert) = revert {
        map.insert("revert".to_owned(), db::decode_json(&revert)?);
    }
    if let Some(outcome) = idle_outcome {
        map.insert("outcome".to_owned(), Value::String(outcome));
    }
    let mut time = Map::new();
    time.insert("created".to_owned(), json!(time_created));
    time.insert("updated".to_owned(), json!(time_updated));
    if let Some(busy) = time_busy {
        time.insert("busy".to_owned(), json!(busy));
    }
    if let Some(idle) = time_idle {
        time.insert("idle".to_owned(), json!(idle));
    }
    if let Some(viewed) = time_viewed {
        time.insert("viewed".to_owned(), json!(viewed));
    }
    if let Some(archived) = time_archived {
        time.insert("archived".to_owned(), json!(archived));
    }
    map.insert("time".to_owned(), Value::Object(time));
    Ok(Value::Object(map))
}

fn json_sql(value: Option<&Value>) -> Result<SqlValue, StoreError> {
    match value {
        Some(Value::Null) | None => Ok(SqlValue::Null),
        Some(value) => Ok(SqlValue::Text(serde_json::to_string(value)?)),
    }
}

/// Fails unless the session row exists.
pub(crate) fn ensure_session(
    transaction: &Transaction<'_>,
    session_id: &str,
) -> Result<(), StoreError> {
    let found: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM session_v2 WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    found.map_or_else(
        || Err(StoreError::NotFound(format!("session {session_id}"))),
        |_| Ok(()),
    )
}

/// Position of a parent session: its root id and its depth.
pub(crate) fn parent_position(
    transaction: &Transaction<'_>,
    parent_id: &str,
) -> Result<Option<(Option<String>, i64)>, StoreError> {
    Ok(transaction
        .query_row(
            "SELECT root_id, depth FROM session_v2 WHERE id = ?1",
            [parent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

fn resolve_root_depth(
    transaction: &Transaction<'_>,
    session_id: &str,
    parent_id: Option<&str>,
    strict: bool,
) -> Result<(String, i64), StoreError> {
    let Some(parent) = parent_id else {
        return Ok((session_id.to_owned(), 0));
    };
    match parent_position(transaction, parent)? {
        Some((root_id, depth)) => Ok((root_id.unwrap_or_else(|| parent.to_owned()), depth + 1)),
        None if strict => Err(StoreError::NotFound(format!("parent session {parent}"))),
        None => Ok((session_id.to_owned(), 0)),
    }
}

/// Deletes the session and every descendant, returning the removed ids.
///
/// The caller decides whether the event stream goes with it: a `session.deleted`
/// event keeps its aggregate so the log can replay the deletion.
pub(crate) fn delete_tree(
    transaction: &Transaction<'_>,
    session_id: &str,
) -> Result<Vec<String>, StoreError> {
    let ids: Vec<String> = {
        let mut statement = transaction.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM session_v2 WHERE id = ?1
                 UNION
                 SELECT child.id FROM session_v2 child JOIN tree ON child.parent_id = tree.id
             )
             SELECT id FROM tree",
        )?;
        let mapped = statement.query_map([session_id], |row| row.get::<_, String>(0))?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };
    if ids.is_empty() {
        return Ok(ids);
    }
    let parent: Option<String> = transaction
        .query_row(
            "SELECT parent_id FROM session_v2 WHERE id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    for id in &ids {
        transaction.execute("DELETE FROM session_v2 WHERE id = ?1", [id])?;
    }
    if let Some(parent) = parent {
        transaction.execute(
            "UPDATE session_v2 SET child_count = max(child_count - 1, 0) WHERE id = ?1",
            [parent],
        )?;
    }
    Ok(ids)
}
