//! Durable forms: pending questions a session asks a client and the answer it
//! gets back.
//!
//! The wire shapes come from `Form.Info` / `Form.Detail` / `Form.CreatePayload`
//! in `packages/protocol/openapi.json`:
//!
//! - [`Store::create_form`] returns `Form.Info` (no state).
//! - [`Store::list_forms`] returns pending `Form.Info` rows, newest first.
//! - [`Store::get_form`] returns `Form.Detail` (`Info` plus `state`).
//! - [`Store::reply_form`] / [`Store::cancel_form`] settle a pending form and
//!   answer `409` (via [`StoreError::Conflict`]) once it is settled.
//!
//! Upstream keeps forms in memory with a ten-minute retention; persisting them
//! means a question survives a restart and can still be answered.

use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

use crate::db::{self, Store, in_tx, now_ms};
use crate::error::StoreError;
use crate::session::ensure_session;

const FORM_COLUMNS: &str = "id, session_id, title, metadata, fields, status, reply, cancel_message";

/// Input for [`Store::create_form`], mirroring `Form.CreatePayload`.
#[derive(Debug, Clone)]
pub struct NewForm {
    /// Requested form id (`frm_…`). When absent the store generates one.
    pub id: Option<String>,
    /// Owning session.
    pub session_id: String,
    /// Display title.
    pub title: String,
    /// Host-supplied metadata object.
    pub metadata: Option<Value>,
    /// `Form.Fields`: a non-empty array of field objects, stored verbatim.
    pub fields: Value,
}

/// One stored form row before it is shaped for the wire.
struct FormRow {
    id: String,
    session_id: String,
    title: String,
    metadata: Option<String>,
    fields: String,
    status: String,
    reply: Option<String>,
    cancel_message: Option<String>,
}

impl FormRow {
    fn read(row: &rusqlite::Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.get(0)?,
            session_id: row.get(1)?,
            title: row.get(2)?,
            metadata: row.get(3)?,
            fields: row.get(4)?,
            status: row.get(5)?,
            reply: row.get(6)?,
            cancel_message: row.get(7)?,
        })
    }

    /// `Form.Info`: `{id, sessionID, title, metadata?, fields}`.
    fn info(&self) -> Result<Value, StoreError> {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String(self.id.clone()));
        map.insert(
            "sessionID".to_owned(),
            Value::String(self.session_id.clone()),
        );
        map.insert("title".to_owned(), Value::String(self.title.clone()));
        if let Some(metadata) = self.metadata.as_deref() {
            map.insert("metadata".to_owned(), db::decode_json(metadata)?);
        }
        map.insert("fields".to_owned(), db::decode_json(&self.fields)?);
        Ok(Value::Object(map))
    }

    /// `Form.Detail`: [`Self::info`] plus `state`.
    fn detail(&self) -> Result<Value, StoreError> {
        let Value::Object(mut map) = self.info()? else {
            return Err(StoreError::Corrupt("form info is not an object".to_owned()));
        };
        map.insert("state".to_owned(), self.state()?);
        Ok(Value::Object(map))
    }

    /// `Form.State`; terminal states are derived from the settled columns.
    fn state(&self) -> Result<Value, StoreError> {
        match self.status.as_str() {
            "pending" => Ok(json!({ "status": "pending" })),
            "replied" => {
                let reply = self.reply.as_deref().ok_or_else(|| {
                    StoreError::Corrupt(format!("form {} is replied without an answer", self.id))
                })?;
                Ok(json!({
                    "status": "answered",
                    "answer": db::decode_json(reply)?,
                }))
            }
            "cancelled" => {
                let mut state = Map::new();
                state.insert("status".to_owned(), Value::String("cancelled".to_owned()));
                if let Some(message) = self.cancel_message.as_deref() {
                    state.insert("message".to_owned(), Value::String(message.to_owned()));
                }
                Ok(Value::Object(state))
            }
            other => Err(StoreError::Corrupt(format!(
                "form {} has unknown status {other:?}",
                self.id
            ))),
        }
    }
}

impl Store {
    /// Creates a form and returns its wire `Form.Info`.
    ///
    /// A duplicate id is a [`StoreError::Conflict`]; `metadata` must be a JSON
    /// object and `fields` a non-empty array. A missing session is
    /// [`StoreError::NotFound`].
    pub async fn create_form(&self, new: NewForm) -> Result<Value, StoreError> {
        let NewForm {
            id,
            session_id,
            title,
            metadata,
            fields,
        } = new;
        let metadata = metadata.filter(|metadata| !metadata.is_null());
        let id = match id {
            Some(id) => {
                if !id.starts_with("frm_") || id.len() == 4 {
                    return Err(StoreError::Invalid(format!(
                        "form id must start with frm_: {id:?}"
                    )));
                }
                id
            }
            None => new_form_id(),
        };
        if let Some(metadata) = metadata.as_ref()
            && !metadata.is_object()
        {
            return Err(StoreError::Invalid(
                "form metadata must be a JSON object".to_owned(),
            ));
        }
        if fields.as_array().is_none_or(Vec::is_empty) {
            return Err(StoreError::Invalid(
                "form fields must be a non-empty array".to_owned(),
            ));
        }
        let metadata = db::encode_json_opt(metadata.as_ref())?;
        let fields = db::encode_json(&fields)?;
        let time = now_ms();
        let id_for_read = id.clone();
        let session_for_read = session_id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                let existing: Option<i64> = transaction
                    .query_row("SELECT 1 FROM form WHERE id = ?1", [&id], |row| row.get(0))
                    .optional()?;
                if existing.is_some() {
                    return Err(StoreError::Conflict(format!("form {id} already exists")));
                }
                transaction.execute(
                    "INSERT INTO form
                        (id, session_id, title, metadata, fields, status,
                         time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
                    params![&id, &session_id, &title, metadata.as_deref(), &fields, time],
                )?;
                Ok(())
            })
        })
        .await?;
        self.form_info(&session_for_read, &id_for_read)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("form {id_for_read}")))
    }

    /// Lists pending forms, newest first.
    ///
    /// `session_id = None` lists every session (the `GET /api/form` route);
    /// `Some` scopes to one session (the per-session route).
    pub async fn list_forms(&self, session_id: Option<&str>) -> Result<Vec<Value>, StoreError> {
        let session_id = session_id.map(str::to_owned);
        self.read(move |connection| {
            let sql = if session_id.is_some() {
                "SELECT id, session_id, title, metadata, fields, status, reply, cancel_message
                 FROM form WHERE status = 'pending' AND session_id = ?1
                 ORDER BY time_created DESC, id DESC"
            } else {
                "SELECT id, session_id, title, metadata, fields, status, reply, cancel_message
                 FROM form WHERE status = 'pending'
                 ORDER BY time_created DESC, id DESC"
            };
            let mut statement = connection.prepare(sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(
                session_id.iter().map(String::as_str),
            ))?;
            let mut forms = Vec::new();
            while let Some(row) = rows.next()? {
                forms.push(FormRow::read(row)?.info()?);
            }
            Ok(forms)
        })
        .await
    }

    /// Loads one form as a wire `Form.Detail` (including `state`).
    ///
    /// Returns [`StoreError::NotFound`] when the id is unknown or belongs to a
    /// different session.
    pub async fn get_form(&self, session_id: &str, id: &str) -> Result<Value, StoreError> {
        self.form_row(session_id, id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("form {id}")))?
            .detail()
    }

    /// Settles a pending form with a `Form.Reply` body (`{"answer": {…}}`).
    ///
    /// An unknown form is [`StoreError::NotFound`]; a form that is already
    /// replied or cancelled is [`StoreError::Conflict`].
    pub async fn reply_form(
        &self,
        session_id: &str,
        id: &str,
        reply: &Value,
    ) -> Result<(), StoreError> {
        let reply = reply.clone();
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                match pending_status(transaction, &session_id, &id)? {
                    PendingStatus::Missing => {
                        return Err(StoreError::NotFound(format!("form {id}")));
                    }
                    PendingStatus::Settled => {
                        return Err(StoreError::Conflict(format!(
                            "form {id} is already settled"
                        )));
                    }
                    PendingStatus::Pending => {}
                }
                let answer = reply
                    .get("answer")
                    .filter(|answer| answer.is_object())
                    .ok_or_else(|| {
                        StoreError::Invalid("form reply must contain an answer object".to_owned())
                    })?;
                let answer = db::encode_json(answer)?;
                transaction.execute(
                    "UPDATE form
                     SET status = 'replied', reply = ?3, cancel_message = NULL,
                         time_updated = ?4
                     WHERE id = ?1 AND session_id = ?2 AND status = 'pending'",
                    params![&id, &session_id, &answer, time],
                )?;
                Ok(())
            })
        })
        .await
    }

    /// Cancels a pending form without a message.
    ///
    /// An unknown form is [`StoreError::NotFound`]; a settled form is
    /// [`StoreError::Conflict`].
    pub async fn cancel_form(&self, session_id: &str, id: &str) -> Result<(), StoreError> {
        self.cancel_form_with_message(session_id, id, None).await
    }

    /// Cancels a pending form, recording the optional `Form.State.cancelled`
    /// `message` the client supplied.
    pub async fn cancel_form_with_message(
        &self,
        session_id: &str,
        id: &str,
        message: Option<&str>,
    ) -> Result<(), StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        let message = message.map(str::to_owned);
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                match pending_status(transaction, &session_id, &id)? {
                    PendingStatus::Missing => {
                        return Err(StoreError::NotFound(format!("form {id}")));
                    }
                    PendingStatus::Settled => {
                        return Err(StoreError::Conflict(format!(
                            "form {id} is already settled"
                        )));
                    }
                    PendingStatus::Pending => {}
                }
                transaction.execute(
                    "UPDATE form
                     SET status = 'cancelled', reply = NULL, cancel_message = ?3,
                         time_updated = ?4
                     WHERE id = ?1 AND session_id = ?2 AND status = 'pending'",
                    params![&id, &session_id, message.as_deref(), time],
                )?;
                Ok(())
            })
        })
        .await
    }

    async fn form_info(&self, session_id: &str, id: &str) -> Result<Option<Value>, StoreError> {
        match self.form_row(session_id, id).await? {
            Some(row) => Ok(Some(row.info()?)),
            None => Ok(None),
        }
    }

    async fn form_row(&self, session_id: &str, id: &str) -> Result<Option<FormRow>, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!("SELECT {FORM_COLUMNS} FROM form WHERE id = ?1 AND session_id = ?2");
            connection
                .query_row(&sql, params![&id, &session_id], |row| {
                    Ok(FormRow::read(row))
                })
                .optional()?
                .transpose()
        })
        .await
    }
}

/// Whether a form id exists and, if so, whether it is still pending.
enum PendingStatus {
    Missing,
    Settled,
    Pending,
}

fn pending_status(
    transaction: &rusqlite::Transaction<'_>,
    session_id: &str,
    id: &str,
) -> Result<PendingStatus, StoreError> {
    let status: Option<String> = transaction
        .query_row(
            "SELECT status FROM form WHERE id = ?1 AND session_id = ?2",
            params![id, session_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match status.as_deref() {
        None => PendingStatus::Missing,
        Some("pending") => PendingStatus::Pending,
        Some(_) => PendingStatus::Settled,
    })
}

/// Generates a `frm_` id with the canonical ascending identifier body.
///
/// `oc-types` has no `FormId`, so the body comes from its ascending `EventId`
/// generator with the prefix swapped; the wire only requires `^frm_`.
fn new_form_id() -> String {
    let event = oc_types::EventId::new();
    format!(
        "frm_{}",
        event
            .as_str()
            .strip_prefix("evt_")
            .unwrap_or(event.as_str())
    )
}
