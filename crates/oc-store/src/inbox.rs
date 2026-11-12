//! Pending input (`session_inbox`).

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{self, Store, in_tx, now_ms};
use crate::error::StoreError;
use crate::message;
use crate::session::ensure_session;

/// How the runner should consume a pending item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Deliver mid-step.
    #[default]
    Steer,
    /// Deliver at the next boundary.
    Queue,
}

impl Delivery {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "steer" => Ok(Self::Steer),
            "queue" => Ok(Self::Queue),
            other => Err(StoreError::Invalid(format!(
                "unknown delivery mode {other:?}"
            ))),
        }
    }
}

/// Input for enqueueing pending input.
#[derive(Debug, Clone)]
pub struct NewInbox {
    /// Message id the item will get when promoted (`msg_…`).
    pub id: String,
    /// Owning session.
    pub session_id: String,
    /// Item discriminator: `user`, `synthetic`, `compaction` or `move`.
    pub kind: String,
    /// Wire payload object.
    pub payload: Value,
    /// Delivery mode.
    pub delivery: Delivery,
}

/// Kinds accepted by the inbox.
pub(crate) const KINDS: [&str; 6] = [
    "user",
    "synthetic",
    "compaction",
    "move",
    "peer",
    "observer",
];

impl Store {
    /// Enqueues pending input. A duplicate id is a `Conflict` error.
    pub async fn enqueue(&self, new: NewInbox) -> Result<Value, StoreError> {
        let NewInbox {
            id,
            session_id,
            kind,
            payload,
            delivery,
        } = new;
        if !payload.is_object() {
            return Err(StoreError::Invalid(
                "inbox payload must be a JSON object".to_owned(),
            ));
        }
        if !KINDS.contains(&kind.as_str()) {
            return Err(StoreError::Invalid(format!("unknown inbox type {kind:?}")));
        }
        let encoded = db::encode_json(&payload)?;
        let time = now_ms();
        let id_for_read = id.clone();
        let session_for_read = session_id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                let existing: Option<i64> = transaction
                    .query_row("SELECT 1 FROM session_inbox WHERE id = ?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if existing.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "inbox item {id} already exists"
                    )));
                }
                let seq = db::next_seq(transaction, &session_id)?;
                transaction.execute(
                    "INSERT INTO session_inbox
                        (id, session_id, type, payload, delivery, enqueued_seq, time_created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        &id,
                        &session_id,
                        &kind,
                        &encoded,
                        delivery.as_str(),
                        seq,
                        time
                    ],
                )?;
                db::touch_session(transaction, &session_id, time)?;
                Ok(())
            })
        })
        .await?;
        self.inbox_item(&session_for_read, &id_for_read)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("inbox item {id_for_read}")))
    }

    /// Lists pending input in enqueue order.
    pub async fn list_inbox(&self, session_id: &str) -> Result<Vec<Value>, StoreError> {
        let session_id = session_id.to_owned();
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, session_id, type, payload, delivery, time_created
                 FROM session_inbox WHERE session_id = ?1 ORDER BY enqueued_seq ASC",
            )?;
            let mut rows = statement.query([&session_id])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(inbox_value(row)?);
            }
            Ok(items)
        })
        .await
    }

    /// Promotes one pending item into a visible message.
    ///
    /// Returns `None` when the item does not exist or is a control item (`move`,
    /// `compaction`) that the runner must handle itself.
    pub async fn promote_inbox(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<Option<Value>, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let row: Option<(String, String, i64)> = transaction
                    .query_row(
                        "SELECT type, payload, time_created FROM session_inbox
                         WHERE id = ?1 AND session_id = ?2",
                        params![&id, &session_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let Some((kind, payload, time_created)) = row else {
                    return Ok(None);
                };
                if kind == "move" || kind == "compaction" {
                    return Ok(None);
                }
                let payload: Value = db::decode_json(&payload)?;
                let body = message::body_with_time(&payload, time_created)?;
                let encoded = db::encode_json(&body)?;
                let seq = db::next_seq(transaction, &session_id)?;
                let removed = transaction.execute(
                    "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id],
                )?;
                if removed == 0 {
                    // Lost the race with another promoter (or the row vanished):
                    // the item is already gone, so inserting here would
                    // materialize a duplicate transcript message. `None` is the
                    // documented "absent item" answer.
                    return Ok(None);
                }
                transaction.execute(
                    "INSERT INTO session_message
                        (id, session_id, type, seq, data, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![&id, &session_id, &kind, seq, &encoded, time_created],
                )?;
                db::touch_session(transaction, &session_id, time_created)?;
                Ok(Some(message::compose(
                    body,
                    &id,
                    &session_id,
                    &kind,
                    time_created,
                )?))
            })
        })
        .await
    }

    /// Cancels one pending item. Returns whether a row was removed.
    pub async fn cancel_inbox(&self, session_id: &str, id: &str) -> Result<bool, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let removed = transaction.execute(
                    "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id],
                )?;
                Ok(removed > 0)
            })
        })
        .await
    }

    /// Changes the delivery mode of one pending item.
    pub async fn set_delivery(
        &self,
        session_id: &str,
        id: &str,
        delivery: Delivery,
    ) -> Result<(), StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let current: Option<String> = transaction
                    .query_row(
                        "SELECT delivery FROM session_inbox WHERE id = ?1 AND session_id = ?2",
                        params![&id, &session_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                match current {
                    None => Err(StoreError::NotFound(format!("inbox item {id}"))),
                    Some(found) if found == delivery.as_str() => Ok(()),
                    Some(_) => {
                        transaction.execute(
                            "UPDATE session_inbox SET delivery = ?3
                             WHERE id = ?1 AND session_id = ?2",
                            params![&id, &session_id, delivery.as_str()],
                        )?;
                        Ok(())
                    }
                }
            })
        })
        .await
    }

    async fn inbox_item(&self, session_id: &str, id: &str) -> Result<Option<Value>, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.read(move |connection| {
            let row = connection
                .query_row(
                    "SELECT id, session_id, type, payload, delivery, time_created
                     FROM session_inbox WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id],
                    |row| Ok(inbox_value(row)),
                )
                .optional()?;
            row.transpose()
        })
        .await
    }
}

fn inbox_value(row: &rusqlite::Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let payload: String = row.get(3)?;
    let delivery: String = row.get(4)?;
    let time_created: i64 = row.get(5)?;
    Delivery::parse(&delivery)?;
    let payload = db::decode_json(&payload)?;
    Ok(serde_json::json!({
        "id": id,
        "sessionID": session_id,
        "type": kind,
        "payload": payload,
        "delivery": delivery,
        "time": { "created": time_created },
    }))
}
