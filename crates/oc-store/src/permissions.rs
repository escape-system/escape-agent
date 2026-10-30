//! Saved permission rules (`permission_saved`).
//!
//! A saved rule is the durable "always allow this action/resource" answer a
//! user gives. The unique triple `(project_id, action, resource)` makes saving
//! idempotent: asking twice returns the same `psv_` row instead of a duplicate.

use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Value, json};

use oc_types::SavedPermissionId;

use crate::db::{Store, in_tx, now_ms};
use crate::error::StoreError;

const SAVED_COLUMNS: &str = "id, project_id, action, resource, time_created, time_updated";

impl Store {
    /// Creates or refreshes a saved rule and returns `PermissionSaved.Info`.
    ///
    /// The first call allocates the `psv_` id and the creation time; later calls
    /// with the same `(project_id, action, resource)` return that row with a
    /// bumped `time.updated`.
    pub async fn save_permission_rule(
        &self,
        project_id: &str,
        action: &str,
        resource: &str,
    ) -> Result<Value, StoreError> {
        validate_rule(project_id, action, resource)?;
        let id = SavedPermissionId::new().into_inner();
        let project_id = project_id.to_owned();
        let action = action.to_owned();
        let resource = resource.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                transaction.execute(
                    "INSERT INTO permission_saved
                        (id, project_id, action, resource, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                     ON CONFLICT(project_id, action, resource)
                     DO UPDATE SET time_updated = max(time_updated, excluded.time_updated)",
                    params![&id, &project_id, &action, &resource, time],
                )?;
                let sql = format!(
                    "SELECT {SAVED_COLUMNS} FROM permission_saved
                     WHERE project_id = ?1 AND action = ?2 AND resource = ?3"
                );
                transaction.query_row(&sql, params![&project_id, &action, &resource], |row| {
                    Ok(saved_value(row))
                })?
            })
        })
        .await
    }

    /// Lists saved rules, newest first, optionally scoped to one project.
    pub async fn list_saved_permissions(
        &self,
        project_id: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        let project_id = project_id.map(str::to_owned);
        self.read(move |connection| {
            let sql = if project_id.is_some() {
                format!(
                    "SELECT {SAVED_COLUMNS} FROM permission_saved
                     WHERE project_id = ?1 ORDER BY time_created DESC, id DESC"
                )
            } else {
                format!(
                    "SELECT {SAVED_COLUMNS} FROM permission_saved
                     ORDER BY time_created DESC, id DESC"
                )
            };
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(
                project_id.iter().map(String::as_str),
            ))?;
            let mut saved = Vec::new();
            while let Some(row) = rows.next()? {
                saved.push(saved_value(row)?);
            }
            Ok(saved)
        })
        .await
    }

    /// Deletes one saved rule. Returns whether a row was removed.
    pub async fn delete_saved_permission(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            let removed =
                connection.execute("DELETE FROM permission_saved WHERE id = ?1", [&id])?;
            Ok(removed > 0)
        })
        .await
    }

    /// Updates the action and/or resource of a saved rule.
    ///
    /// `Ok(None)` means the id is absent (or belongs to another project when
    /// `project_id` is given). Changing a rule into an existing
    /// `(project, action, resource)` triple is [`StoreError::Conflict`].
    pub async fn update_saved_permission(
        &self,
        id: &str,
        project_id: Option<&str>,
        action: Option<&str>,
        resource: Option<&str>,
    ) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        let project_id = project_id.map(str::to_owned);
        let action = action.map(str::to_owned);
        let resource = resource.map(str::to_owned);
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let current = transaction
                    .query_row(
                        "SELECT id, project_id, action, resource, time_created, time_updated
                         FROM permission_saved WHERE id = ?1",
                        [&id],
                        |row| Ok(saved_value(row)),
                    )
                    .optional()?
                    .transpose()?;
                let Some(current) = current else {
                    return Ok(None);
                };
                let current_project = current["projectID"].as_str().unwrap_or_default();
                if project_id
                    .as_deref()
                    .is_some_and(|project| project != current_project)
                {
                    return Ok(None);
                }
                let next_action = action
                    .as_deref()
                    .unwrap_or_else(|| current["action"].as_str().unwrap_or_default());
                let next_resource = resource
                    .as_deref()
                    .unwrap_or_else(|| current["resource"].as_str().unwrap_or_default());
                validate_rule(current_project, next_action, next_resource)?;
                let duplicate: Option<i64> = transaction
                    .query_row(
                        "SELECT 1 FROM permission_saved
                         WHERE project_id = ?1 AND action = ?2 AND resource = ?3
                           AND id <> ?4",
                        params![current_project, next_action, next_resource, &id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "saved permission {current_project}/{next_action}/{next_resource} \
                         already exists"
                    )));
                }
                transaction
                    .execute(
                        "UPDATE permission_saved
                         SET action = ?2, resource = ?3, time_updated = ?4
                         WHERE id = ?1",
                        params![&id, next_action, next_resource, time],
                    )
                    .map_err(conflict_on_unique)?;
                let updated = transaction
                    .query_row(
                        "SELECT id, project_id, action, resource, time_created, time_updated
                         FROM permission_saved WHERE id = ?1",
                        [&id],
                        |row| Ok(saved_value(row)),
                    )
                    .optional()?
                    .transpose()?;
                Ok(updated)
            })
        })
        .await
    }
}

fn validate_rule(project_id: &str, action: &str, resource: &str) -> Result<(), StoreError> {
    if project_id.is_empty() || action.is_empty() || resource.is_empty() {
        return Err(StoreError::Invalid(
            "saved permission project, action and resource must not be empty".to_owned(),
        ));
    }
    Ok(())
}

/// `PermissionSaved.Info`: `{id, projectID, action, resource, time:{created,updated}}`.
fn saved_value(row: &Row<'_>) -> Result<Value, StoreError> {
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "projectID": row.get::<_, String>(1)?,
        "action": row.get::<_, String>(2)?,
        "resource": row.get::<_, String>(3)?,
        "time": {
            "created": row.get::<_, i64>(4)?,
            "updated": row.get::<_, i64>(5)?,
        },
    }))
}

/// Maps a SQLite unique-constraint failure to [`StoreError::Conflict`].
fn conflict_on_unique(error: rusqlite::Error) -> StoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
        && matches!(failure.extended_code, 1555 | 2067)
    {
        return StoreError::Conflict("saved permission already exists".to_owned());
    }
    StoreError::Sqlite(error)
}
