//! Worktree rows: the per-project checkouts registered with the server.
//!
//! The upstream wire object (`Worktree.Info`) is only `{ directory }`, so the
//! store keeps the extra bookkeeping columns (`canonical`, `branch`, `name`,
//! timestamps) internal and emits just the directory. A directory is unique per
//! project; ids are generated when the caller does not supply one.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::{Store, generate_id, in_tx, now_ms};
use crate::error::StoreError;

/// Fields for a new worktree row. Timestamps default to the current time and
/// the id is generated when absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewWorktree {
    /// Wire id; generated when absent.
    pub id: Option<String>,
    /// Owning project; must already exist.
    pub project_id: String,
    /// Absolute checkout directory.
    pub directory: String,
    /// Canonicalised directory when the caller resolved one.
    pub canonical: Option<String>,
    /// Git branch checked out in the worktree.
    pub branch: Option<String>,
    /// Display name.
    pub name: Option<String>,
    /// Creation timestamp in milliseconds; defaults to now.
    pub time_created: Option<i64>,
    /// Update timestamp in milliseconds; defaults to `time_created`.
    pub time_updated: Option<i64>,
}

impl Store {
    /// Lists worktrees newest first, optionally restricted to one project.
    ///
    /// # Errors
    ///
    /// Fails only when the database read fails.
    pub async fn list_worktrees(&self, project_id: Option<&str>) -> Result<Vec<Value>, StoreError> {
        let project_id = project_id.map(str::to_owned);
        self.read(move |connection| {
            let filter = if project_id.is_some() {
                " WHERE project_id = ?1"
            } else {
                ""
            };
            let sql = format!(
                "SELECT directory FROM worktree{filter}
                 ORDER BY time_created DESC, directory ASC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = match project_id.as_deref() {
                Some(project_id) => statement.query([project_id])?,
                None => statement.query([])?,
            };
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(json!({ "directory": row.get::<_, String>(0)? }));
            }
            Ok(items)
        })
        .await
    }

    /// Inserts a worktree and returns the wire `Worktree.Info` object.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when the project row is missing,
    /// [`StoreError::Conflict`] for a duplicate id or a directory already
    /// registered to the same project, and [`StoreError::Invalid`] for an empty
    /// directory.
    pub async fn create_worktree(&self, input: NewWorktree) -> Result<Value, StoreError> {
        if input.directory.trim().is_empty() {
            return Err(StoreError::Invalid(
                "worktree directory is empty".to_owned(),
            ));
        }
        let NewWorktree {
            id,
            project_id,
            directory,
            canonical,
            branch,
            name,
            time_created,
            time_updated,
        } = input;
        let id = id.unwrap_or_else(|| generate_id("wkt_"));
        let created = time_created.unwrap_or_else(now_ms);
        let updated = time_updated.unwrap_or(created);
        let directory_for_wire = directory.clone();

        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let project: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM project WHERE id = ?1",
                        [&project_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if project.is_none() {
                    return Err(StoreError::NotFound(format!("project {project_id}")));
                }
                let duplicate: Option<String> = transaction
                    .query_row("SELECT id FROM worktree WHERE id = ?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "worktree already exists: {id}"
                    )));
                }
                let duplicate: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM worktree WHERE project_id = ?1 AND directory = ?2",
                        params![project_id, directory],
                        |row| row.get(0),
                    )
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "worktree directory already registered: {directory}"
                    )));
                }
                transaction.execute(
                    "INSERT INTO worktree
                        (id, project_id, directory, canonical, branch, name, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![id, project_id, directory, canonical, branch, name, created, updated],
                )?;
                Ok(())
            })
        })
        .await?;

        Ok(json!({ "directory": directory_for_wire }))
    }

    /// Loads one worktree as a wire `Worktree.Info` object.
    pub async fn get_worktree(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let row = connection
                .query_row(
                    "SELECT directory FROM worktree WHERE id = ?1",
                    [&id],
                    |row| Ok(worktree_value(row)),
                )
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Deletes a worktree row and reports whether it existed.
    pub async fn delete_worktree(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            Ok(connection.execute("DELETE FROM worktree WHERE id = ?1", [&id])? > 0)
        })
        .await
    }

    /// Deletes the worktree registered at `directory` for `project_id`.
    ///
    /// The wire remove route addresses worktrees by project and directory, not
    /// by id. Returns whether a row was removed.
    pub async fn delete_worktree_by_directory(
        &self,
        project_id: &str,
        directory: &str,
    ) -> Result<bool, StoreError> {
        let project_id = project_id.to_owned();
        let directory = directory.to_owned();
        self.write(move |connection| {
            Ok(connection.execute(
                "DELETE FROM worktree WHERE project_id = ?1 AND directory = ?2",
                params![project_id, directory],
            )? > 0)
        })
        .await
    }
}

fn worktree_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let directory: String = row.get(0)?;
    Ok(json!({ "directory": directory }))
}
