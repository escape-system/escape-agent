//! Stored credentials for integrations.
//!
//! A credential is a labelled secret value (`oauth`, `key` or `external`) owned
//! by an integration. Each integration selects at most one credential as
//! active; activating a credential clears the flag on the integration's other
//! rows in the same transaction. Values are opaque wire JSON and are emitted
//! exactly as stored.

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Row, Transaction, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::{Store, generate_id, in_tx};
use crate::error::StoreError;

const CREDENTIAL_COLUMNS: &str = "id, integration_id, label, \"value\", active";

/// Fields for a new credential. `None` for `id`/`label`/`active` takes the
/// upstream defaults: a generated `cred_` id, the label `default`, and
/// activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCredential {
    /// Wire id; generated when absent.
    pub id: Option<String>,
    /// Integration that owns the credential.
    pub integration_id: String,
    /// Human label; defaults to `default`.
    pub label: Option<String>,
    /// Tagged secret value (`{ "type": "oauth" | "key" | "external", … }`).
    pub value: Value,
    /// `Some(false)` keeps the integration's current selection; `None` and
    /// `Some(true)` make the new credential active.
    pub active: Option<bool>,
}

/// Fields to change on a credential. Absent fields are left untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CredentialPatch {
    /// New display label.
    pub label: Option<String>,
    /// New tagged secret value.
    pub value: Option<Value>,
    /// Select or deselect the credential for its integration.
    pub active: Option<bool>,
}

/// One stored credential row, decoded for callers that need identity and
/// secret material rather than the wire shape.
///
/// Ordered pools ([`Store::credential_pool`]) return these so key rotation sees
/// stable ids and the activation flag without re-parsing wire JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRow {
    /// Credential id (`cred_…`).
    pub id: String,
    /// Integration that owns the credential.
    pub integration_id: String,
    /// Human label; `default` when the row never had one.
    pub label: String,
    /// Tagged secret value (`{ "type": "oauth" | "key" | "external", … }`).
    pub value: Value,
    /// Whether this is the integration's current selection.
    pub active: bool,
}

impl Store {
    /// Inserts a credential and returns it as a wire `Credential.Entry`.
    ///
    /// The credential becomes active when requested (the default) or when it is
    /// the first credential for its integration. The integration-wide
    /// activation clear and the insert share one transaction.
    ///
    /// # Errors
    ///
    /// Fails with [`StoreError::Conflict`] when the id exists and
    /// [`StoreError::Invalid`] when the integration id is empty or the value is
    /// not a tagged credential object.
    pub async fn create_credential(&self, input: NewCredential) -> Result<Value, StoreError> {
        if input.integration_id.trim().is_empty() {
            return Err(StoreError::Invalid(
                "credential integrationID is empty".to_owned(),
            ));
        }
        validate_value(&input.value)?;
        let id = input.id.unwrap_or_else(|| generate_id("cred_"));
        let integration_id = input.integration_id;
        let label = input.label.unwrap_or_else(|| "default".to_owned());
        let value = serde_json::to_string(&input.value)?;
        let requested_active = input.active.unwrap_or(true);

        let id_for_read = id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let duplicate: Option<String> = transaction
                    .query_row("SELECT id FROM credential WHERE id = ?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "credential already exists: {id}"
                    )));
                }
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM credential WHERE integration_id = ?1 LIMIT 1",
                        [&integration_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let active = requested_active || existing.is_none();
                if active {
                    transaction.execute(
                        "UPDATE credential SET active = 0 WHERE integration_id = ?1",
                        [&integration_id],
                    )?;
                } else {
                    // Legacy rows may have no flag at all; pin the newest one
                    // before inserting a newer, inactive credential.
                    let selected: Option<String> = transaction
                        .query_row(
                            "SELECT id FROM credential
                                 WHERE integration_id = ?1 AND active = 1 LIMIT 1",
                            [&integration_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if selected.is_none() {
                        let newest: Option<String> = transaction
                            .query_row(
                                "SELECT id FROM credential WHERE integration_id = ?1
                                     ORDER BY id DESC LIMIT 1",
                                [&integration_id],
                                |row| row.get(0),
                            )
                            .optional()?;
                        if let Some(newest) = newest {
                            transaction.execute(
                                "UPDATE credential SET active = 1 WHERE id = ?1",
                                [&newest],
                            )?;
                        }
                    }
                }
                transaction.execute(
                    "INSERT INTO credential (id, integration_id, label, \"value\", active)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, integration_id, label, value, i64::from(active)],
                )?;
                Ok(())
            })
        })
        .await?;
        self.get_credential(&id_for_read)
            .await?
            .ok_or_else(|| StoreError::Corrupt(format!("credential {id_for_read} vanished")))
    }

    /// Lists every credential ordered by integration and then id (ascending ids
    /// encode creation time).
    pub async fn list_credentials(&self) -> Result<Vec<Value>, StoreError> {
        self.read(|connection| {
            let sql = format!(
                "SELECT {CREDENTIAL_COLUMNS} FROM credential
                 ORDER BY integration_id ASC, id ASC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                if let Some(entry) = credential_value(row)? {
                    items.push(entry);
                }
            }
            Ok(items)
        })
        .await
    }

    /// Loads one credential as a wire `Credential.Entry`. Rows without an
    /// integration (legacy shape) are treated as absent.
    pub async fn get_credential(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {CREDENTIAL_COLUMNS} FROM credential
                 WHERE id = ?1 AND integration_id IS NOT NULL"
            );
            let row = connection
                .query_row(&sql, [&id], |row| Ok(credential_value(row)))
                .optional()?;
            match row {
                None => Ok(None),
                Some(entry) => entry,
            }
        })
        .await
    }

    /// Applies a patch and returns the updated entry, or `None` when the id is
    /// unknown. Setting `active` moves the integration's selection atomically.
    pub async fn update_credential(
        &self,
        id: &str,
        patch: CredentialPatch,
    ) -> Result<Option<Value>, StoreError> {
        if let Some(value) = &patch.value {
            validate_value(value)?;
        }
        let id = id.to_owned();
        let id_for_read = id.clone();
        let found = self
            .write(move |connection| {
                in_tx(connection, move |transaction| {
                    let integration: Option<String> = transaction
                        .query_row(
                            "SELECT integration_id FROM credential
                             WHERE id = ?1 AND integration_id IS NOT NULL",
                            [&id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    let Some(integration) = integration else {
                        return Ok(false);
                    };
                    let mut assignments: Vec<&'static str> = Vec::new();
                    let mut values: Vec<SqlValue> = Vec::new();
                    if let Some(label) = patch.label {
                        assignments.push("label = ?");
                        values.push(SqlValue::Text(label));
                    }
                    if let Some(value) = patch.value {
                        assignments.push("\"value\" = ?");
                        values.push(SqlValue::Text(serde_json::to_string(&value)?));
                    }
                    if let Some(active) = patch.active {
                        if active {
                            transaction.execute(
                                "UPDATE credential SET active = 0
                                 WHERE integration_id = ?1 AND id <> ?2",
                                params![integration, id],
                            )?;
                        }
                        assignments.push("active = ?");
                        values.push(SqlValue::Integer(i64::from(active)));
                    }
                    if !assignments.is_empty() {
                        values.push(SqlValue::Text(id));
                        let sql = format!(
                            "UPDATE credential SET {} WHERE id = ?",
                            assignments.join(", ")
                        );
                        transaction.execute(&sql, params_from_iter(values.iter()))?;
                    }
                    Ok(true)
                })
            })
            .await?;
        if !found {
            return Ok(None);
        }
        self.get_credential(&id_for_read).await
    }

    /// Deletes a credential. Returns whether a row was removed; the
    /// integration's other credentials are untouched.
    pub async fn delete_credential(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            Ok(connection.execute("DELETE FROM credential WHERE id = ?1", [&id])? > 0)
        })
        .await
    }

    /// Selects a credential for its integration and clears the flag on every
    /// sibling in one transaction.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when the id is unknown.
    pub async fn activate_credential(&self, id: &str) -> Result<Value, StoreError> {
        let id = id.to_owned();
        let id_for_read = id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                if activate_in_tx(transaction, &id)? {
                    Ok(())
                } else {
                    Err(StoreError::NotFound(format!("credential {id}")))
                }
            })
        })
        .await?;
        self.get_credential(&id_for_read)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("credential {id_for_read}")))
    }

    /// Every credential of one integration ordered by rotation priority: the
    /// active one first, then the remaining ids ascending.
    ///
    /// This is the single source of truth for backup-key order. Ids encode
    /// creation time, so the order is stable across restarts without any extra
    /// column.
    pub async fn credential_pool(
        &self,
        integration_id: &str,
    ) -> Result<Vec<CredentialRow>, StoreError> {
        let integration_id = integration_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {CREDENTIAL_COLUMNS} FROM credential
                 WHERE integration_id = ?1
                 ORDER BY active DESC, id ASC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([&integration_id])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                if let Some(entry) = credential_row(row)? {
                    items.push(entry);
                }
            }
            Ok(items)
        })
        .await
    }

    /// Makes `credential_id` the active credential of `integration_id` and
    /// clears the flag on its siblings, in one transaction.
    ///
    /// Returns `false` when the id is unknown or belongs to a different
    /// integration; the stored selection is then untouched. Rotation calls this
    /// after a key starts cooling down so a restart does not pick the dead key
    /// first; it shares [`activate_credential`](Self::activate_credential)'s
    /// transaction body, so kill -9 safety is identical.
    pub async fn set_credential_primary(
        &self,
        integration_id: &str,
        credential_id: &str,
    ) -> Result<bool, StoreError> {
        let integration_id = integration_id.to_owned();
        let credential_id = credential_id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let owner: Option<Option<String>> = transaction
                    .query_row(
                        "SELECT integration_id FROM credential WHERE id = ?1",
                        [&credential_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                match owner {
                    Some(Some(owner)) if owner == integration_id => {
                        activate_in_tx(transaction, &credential_id)
                    }
                    _ => Ok(false),
                }
            })
        })
        .await
    }
}

/// Shared body of [`Store::activate_credential`] and
/// [`Store::set_credential_primary`]: resolve the owning integration, clear
/// every sibling flag, then set exactly one row. `Ok(false)` means the id is
/// unknown; nothing was written.
fn activate_in_tx(transaction: &Transaction<'_>, id: &str) -> Result<bool, StoreError> {
    let integration: Option<String> = transaction
        .query_row(
            "SELECT integration_id FROM credential
             WHERE id = ?1 AND integration_id IS NOT NULL",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(integration) = integration else {
        return Ok(false);
    };
    transaction.execute(
        "UPDATE credential SET active = 0 WHERE integration_id = ?1",
        [&integration],
    )?;
    transaction.execute("UPDATE credential SET active = 1 WHERE id = ?1", [id])?;
    Ok(true)
}

fn validate_value(value: &Value) -> Result<(), StoreError> {
    let Some(tag) = value.get("type").and_then(Value::as_str) else {
        return Err(StoreError::Invalid(
            "credential value must be an object with a string type".to_owned(),
        ));
    };
    if !matches!(tag, "oauth" | "key" | "external") {
        return Err(StoreError::Invalid(format!(
            "credential type must be oauth, key or external: {tag}"
        )));
    }
    Ok(())
}

/// Decodes one row into [`CredentialRow`]. `Ok(None)` means the row has no
/// integration id, which no pool query can return but legacy rows can.
fn credential_row(row: &Row<'_>) -> Result<Option<CredentialRow>, StoreError> {
    let id: String = row.get(0)?;
    let integration_id: Option<String> = row.get(1)?;
    let Some(integration_id) = integration_id else {
        return Ok(None);
    };
    let label: Option<String> = row.get(2)?;
    let encoded: Option<String> = row.get(3)?;
    let active: Option<i64> = row.get(4)?;
    let encoded =
        encoded.ok_or_else(|| StoreError::Corrupt(format!("credential {id} has no value")))?;
    let value: Value = serde_json::from_str(&encoded)
        .map_err(|error| StoreError::Corrupt(format!("credential {id} value: {error}")))?;
    Ok(Some(CredentialRow {
        id,
        integration_id,
        label: label.unwrap_or_else(|| "default".to_owned()),
        value,
        active: active.is_some_and(|flag| flag != 0),
    }))
}

/// Decodes one row. `Ok(None)` means the row has no integration id, which the
/// wire cannot represent; callers filter those out.
fn credential_value(row: &Row<'_>) -> Result<Option<Value>, StoreError> {
    let id: String = row.get(0)?;
    let integration_id: Option<String> = row.get(1)?;
    let Some(integration_id) = integration_id else {
        return Ok(None);
    };
    let label: Option<String> = row.get(2)?;
    let encoded: Option<String> = row.get(3)?;
    let active: Option<i64> = row.get(4)?;
    let encoded =
        encoded.ok_or_else(|| StoreError::Corrupt(format!("credential {id} has no value")))?;
    let value: Value = serde_json::from_str(&encoded)
        .map_err(|error| StoreError::Corrupt(format!("credential {id} value: {error}")))?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("integrationID".to_owned(), Value::String(integration_id));
    map.insert(
        "label".to_owned(),
        Value::String(label.unwrap_or_else(|| "default".to_owned())),
    );
    map.insert(
        "active".to_owned(),
        Value::Bool(active.is_some_and(|flag| flag != 0)),
    );
    map.insert("value".to_owned(), value);
    Ok(Some(Value::Object(map)))
}
