//! API-managed instruction entries (`instruction_entry`).
//!
//! Values are arbitrary JSON, stored as their JSON text. Clients can attach,
//! replace and remove an entry at any time; the runtime renders the active set
//! into the model's context.
//!
//! [`Store::instruction_entries`] returns only active rows (`removed = 0`) in
//! key order, shaped exactly like wire `InstructionEntry.Info`
//! (`{key, value}`); removals are recorded by deleting the row.

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use crate::db::{Store, in_tx};
use crate::error::StoreError;
use crate::session::ensure_session;

impl Store {
    /// Lists the session's active instruction entries in key order.
    ///
    /// The stored text is decoded as JSON; text that is not valid JSON (a value
    /// written by an older caller) is returned as a JSON string so it is never
    /// lost. Rows marked `removed` are excluded, matching the wire list route.
    pub async fn instruction_entries(&self, session_id: &str) -> Result<Vec<Value>, StoreError> {
        let session_id = session_id.to_owned();
        self.read(move |connection| {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT 1 FROM session_v2 WHERE id = ?1",
                    [&session_id],
                    |row| row.get(0),
                )
                .optional()?;
            if found.is_none() {
                return Err(StoreError::NotFound(format!("session {session_id}")));
            }
            let mut statement = connection.prepare(
                "SELECT \"key\", \"value\" FROM instruction_entry
                 WHERE session_id = ?1 AND removed = 0
                 ORDER BY \"key\" ASC",
            )?;
            let mut rows = statement.query([&session_id])?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let value: Option<String> = row.get(1)?;
                entries.push(json!({
                    "key": key,
                    "value": decode_instruction(value.as_deref()),
                }));
            }
            Ok(entries)
        })
        .await
    }

    /// Attaches or replaces one instruction entry.
    ///
    /// `Some` stores `value` verbatim (expected to be JSON text serialized by
    /// the caller) with `removed = 0`; `None` deletes the row. Both forms are
    /// idempotent.
    pub async fn set_instruction_entry(
        &self,
        session_id: &str,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), StoreError> {
        if key.is_empty() {
            return Err(StoreError::Invalid(
                "instruction entry key must not be empty".to_owned(),
            ));
        }
        let session_id = session_id.to_owned();
        let key = key.to_owned();
        let value = value.map(str::to_owned);
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                match value.as_deref() {
                    Some(value) => {
                        transaction.execute(
                            "INSERT INTO instruction_entry
                                (session_id, \"key\", \"value\", removed)
                             VALUES (?1, ?2, ?3, 0)
                             ON CONFLICT(session_id, \"key\")
                             DO UPDATE SET \"value\" = excluded.\"value\", removed = 0",
                            params![&session_id, &key, value],
                        )?;
                    }
                    None => {
                        transaction.execute(
                            "DELETE FROM instruction_entry
                             WHERE session_id = ?1 AND \"key\" = ?2",
                            params![&session_id, &key],
                        )?;
                    }
                }
                Ok(())
            })
        })
        .await
    }
}

fn decode_instruction(text: Option<&str>) -> Value {
    match text {
        None => Value::Null,
        Some(text) => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())),
    }
}
