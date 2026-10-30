//! Project rows: the foreign-key parent of sessions, plus the UI routes that
//! list, read and patch them.
//!
//! Sessions reference a project (`session_v2.project_id` is a foreign key with
//! cascade), so every session write ensures the owning project row exists. The
//! project id is derived by the caller (`project_id_from_directory`).

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::db::{Store, in_tx, now_ms};
use crate::error::StoreError;

const PROJECT_COLUMNS: &str =
    "id, directory, canonical, name, icon, time_created, time_updated, archived";

/// Fields to change on a project. Absent fields are left untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProjectPatch {
    /// New display name.
    pub name: Option<String>,
    /// New icon URL.
    pub icon: Option<String>,
    /// Archive or unarchive the project.
    pub archived: Option<bool>,
}

/// Inserts the project row when it is missing; existing rows are left untouched.
pub(crate) fn ensure_project(
    transaction: &Transaction<'_>,
    id: &str,
    directory: &str,
    time: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT OR IGNORE INTO project (id, directory, canonical, time_created, time_updated)
         VALUES (?1, ?2, ?2, ?3, ?3)",
        params![id, directory, time],
    )?;
    Ok(())
}

impl Store {
    /// Lists projects, newest update first. `archived` filters the flag; `None`
    /// returns archived and live projects together.
    pub async fn list_projects(&self, archived: Option<bool>) -> Result<Vec<Value>, StoreError> {
        self.read(move |connection| {
            let filter = match archived {
                Some(true) => " WHERE archived = 1",
                Some(false) => " WHERE archived = 0",
                None => "",
            };
            let sql = format!(
                "SELECT {PROJECT_COLUMNS} FROM project{filter} ORDER BY time_updated DESC, id DESC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(project_value(row)?);
            }
            Ok(items)
        })
        .await
    }

    /// Registers a project discovered by a location request.
    ///
    /// A new id inserts a row with the current time as both timestamps. When
    /// the id already exists only `directory` and `canonical` are refreshed, so
    /// a user-set `name`/`icon` and the original creation time survive.
    /// Returns the stored row.
    pub async fn register_project(
        &self,
        id: &str,
        directory: &str,
        canonical: &str,
    ) -> Result<Value, StoreError> {
        if id.trim().is_empty() {
            return Err(StoreError::Invalid("a project id is required".to_owned()));
        }
        if directory.trim().is_empty() {
            return Err(StoreError::Invalid(
                "a project directory is required".to_owned(),
            ));
        }
        let id = id.to_owned();
        let directory = directory.to_owned();
        let canonical = canonical.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                transaction.execute(
                    "INSERT INTO project (id, directory, canonical, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?4)
                     ON CONFLICT(id) DO UPDATE SET
                         directory = excluded.directory,
                         canonical = excluded.canonical",
                    params![&id, &directory, &canonical, now_ms()],
                )?;
                let sql = format!("SELECT {PROJECT_COLUMNS} FROM project WHERE id = ?1");
                let row = transaction
                    .query_row(&sql, [&id], |row| Ok(project_value(row)))
                    .optional()?;
                match row {
                    Some(row) => row,
                    None => Err(StoreError::NotFound(format!("project not found: {id}"))),
                }
            })
        })
        .await
    }

    /// Loads one project as a wire `Project.Info` object.
    pub async fn get_project(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!("SELECT {PROJECT_COLUMNS} FROM project WHERE id = ?1");
            let row = connection
                .query_row(&sql, [&id], |row| Ok(project_value(row)))
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Applies a patch to a project and returns the updated object, or `None`
    /// when the id is unknown. `time_updated` is always bumped.
    pub async fn update_project(
        &self,
        id: &str,
        patch: ProjectPatch,
    ) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        let id_for_read = id.clone();
        let updated = now_ms();
        let changed = self
            .write(move |connection| {
                in_tx(connection, |transaction| {
                    let mut assignments: Vec<&'static str> = Vec::new();
                    let mut values: Vec<SqlValue> = Vec::new();
                    if let Some(name) = &patch.name {
                        assignments.push("name = ?");
                        values.push(SqlValue::Text(name.clone()));
                    }
                    if let Some(icon) = &patch.icon {
                        assignments.push("icon = ?");
                        values.push(SqlValue::Text(icon.clone()));
                    }
                    if let Some(archived) = patch.archived {
                        assignments.push("archived = ?");
                        values.push(SqlValue::Integer(i64::from(archived)));
                    }
                    assignments.push("time_updated = ?");
                    values.push(SqlValue::Integer(updated));
                    values.push(SqlValue::Text(id));
                    let sql = format!("UPDATE project SET {} WHERE id = ?", assignments.join(", "));
                    Ok(transaction.execute(&sql, params_from_iter(values.iter()))? > 0)
                })
            })
            .await?;
        if !changed {
            return Ok(None);
        }
        self.get_project(&id_for_read).await
    }
}

fn project_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let directory: String = row.get(1)?;
    let canonical: String = row.get(2)?;
    let name: Option<String> = row.get(3)?;
    let icon: Option<String> = row.get(4)?;
    let time_created: Option<i64> = row.get(5)?;
    let time_updated: Option<i64> = row.get(6)?;
    let archived: i64 = row.get(7)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("directory".to_owned(), Value::String(directory));
    map.insert("canonical".to_owned(), Value::String(canonical));
    if let Some(name) = name {
        map.insert("name".to_owned(), Value::String(name));
    }
    if let Some(icon) = icon {
        map.insert("icon".to_owned(), Value::String(icon));
    }
    map.insert(
        "time".to_owned(),
        json!({
            "created": time_created.unwrap_or(0),
            "updated": time_updated.unwrap_or(0),
        }),
    );
    if archived != 0 {
        map.insert("archived".to_owned(), Value::Bool(true));
    }
    Ok(Value::Object(map))
}
