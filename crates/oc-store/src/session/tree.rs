//! Session tree helpers: parent positions, root/depth resolution, creation with
//! explicit tree columns and recursive deletion.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::db::{self, Store, in_tx};
use crate::error::StoreError;
use crate::project::ensure_project;

use super::{NewSession, NewSessionTree};

impl Store {
    pub(super) async fn create_session_inner(
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
        transaction.execute(
            "DELETE FROM memory WHERE scope = 'session' AND scope_id = ?1",
            [id],
        )?;
        transaction.execute("DELETE FROM goal WHERE root_id = ?1", [id])?;
        transaction.execute(
            "DELETE FROM observation WHERE root_id = ?1 OR session_id = ?1",
            [id],
        )?;
        transaction.execute(
            "DELETE FROM observer_repair WHERE root_id = ?1 OR target_id = ?1",
            [id],
        )?;
        transaction.execute("DELETE FROM permission_request WHERE session_id = ?1", [id])?;
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

impl Store {
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
