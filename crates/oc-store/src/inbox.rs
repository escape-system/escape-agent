//! Pending input (`session_inbox`).

use oc_types::MessageId;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

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

/// Message type a promoted inbox row gets.
///
/// `peer` and `observer` are additive inbox kinds, but the wire message union
/// (`SessionMessageInfo`) is `user|synthetic|system|skill|shell|assistant|
/// compaction|idle|agent-switched|model-switched|location-switched`. Rows of a
/// kind the wire cannot carry are promoted as `synthetic`; the sender metadata
/// already lives in the payload (`metadata.source`, `from`, `agent`, `kind`), so
/// no information is lost and the client can render the row instead of
/// dropping an unknown type. Legacy `peer` rows written before this rule are
/// normalized by the same function.
pub(crate) fn message_kind(kind: &str) -> &str {
    match kind {
        "peer" | "observer" => "synthetic",
        other => other,
    }
}

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

/// A persisted `session_inbox` row.
///
/// [`Store::enqueue_materialized`] returns it together with the transcript
/// message it committed, so a caller can describe the queued item from the
/// values the database holds instead of re-reading them.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxRow {
    /// Inbox item id (also the id of the materialised message).
    pub id: String,
    /// Owning session.
    pub session_id: String,
    /// Stored item discriminator (`user`, `synthetic`, `compaction`, `move`,
    /// `peer` or `observer`).
    pub kind: String,
    /// Wire payload object.
    pub payload: Value,
    /// Delivery mode.
    pub delivery: Delivery,
    /// Enqueue time in epoch milliseconds; also the materialised message's
    /// `time.created`.
    pub time_created: i64,
}

impl InboxRow {
    /// The message id this item becomes.
    #[must_use]
    pub fn message_id(&self) -> MessageId {
        MessageId::from_wire(self.id.clone())
    }

    /// The wire `Session.Inbox.Info` object, byte-identical to what
    /// [`Store::enqueue`] returns (legacy `peer`/`observer` kinds are surfaced
    /// as the wire-valid kind they materialise to).
    #[must_use]
    pub fn to_wire(&self) -> Value {
        json!({
            "id": self.id,
            "sessionID": self.session_id,
            "type": message_kind(&self.kind),
            "payload": self.payload,
            "delivery": self.delivery.as_str(),
            "time": { "created": self.time_created },
        })
    }
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

/// Kinds that carry a transcript message; the other two are control items.
fn materializable(kind: &str) -> bool {
    kind != "move" && kind != "compaction"
}

/// The shared admission validation: a JSON-object payload and a known kind.
fn validate_inbox(item: &NewInbox) -> Result<(), StoreError> {
    if !item.payload.is_object() {
        return Err(StoreError::Invalid(
            "inbox payload must be a JSON object".to_owned(),
        ));
    }
    if !KINDS.contains(&item.kind.as_str()) {
        return Err(StoreError::Invalid(format!(
            "unknown inbox type {:?}",
            item.kind
        )));
    }
    Ok(())
}

/// The single inbox-insert code path used by every admission method.
///
/// It validates nothing itself — callers run [`validate_inbox`] first — so a
/// new admission variant cannot drift from the duplicate check, the sequence
/// allocation order or the session watermark.
fn insert_inbox_row(
    transaction: &Transaction<'_>,
    id: &str,
    session_id: &str,
    kind: &str,
    encoded_payload: &str,
    delivery: Delivery,
    time: i64,
) -> Result<(), StoreError> {
    ensure_session(transaction, session_id)?;
    let existing: Option<i64> = transaction
        .query_row("SELECT 1 FROM session_inbox WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    if existing.is_some() {
        return Err(StoreError::Conflict(format!(
            "inbox item {id} already exists"
        )));
    }
    let seq = db::next_seq(transaction, session_id)?;
    transaction.execute(
        "INSERT INTO session_inbox
            (id, session_id, type, payload, delivery, enqueued_seq, time_created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            session_id,
            kind,
            encoded_payload,
            delivery.as_str(),
            seq,
            time
        ],
    )?;
    db::touch_session(transaction, session_id, time)?;
    Ok(())
}

/// Build the transcript body for a materialised inbox row.
///
/// Same shape [`Store::promote_inbox`] produces (payload plus `time.created`),
/// with the delivery bookkeeping added under `metadata`: `inboxID` (the
/// promotion anchor), `delivered: false` and, for a `peer` row, `source:
/// "peer"`. Existing payload metadata is preserved; a payload that carries a
/// non-object `metadata` cannot hold the anchor and is rejected instead of
/// being silently reshaped.
fn materialized_body(
    payload: &Value,
    inbox_id: &str,
    kind: &str,
    time: i64,
) -> Result<Value, StoreError> {
    let Value::Object(mut map) = payload.clone() else {
        return Err(StoreError::Invalid(
            "inbox payload must be a JSON object".to_owned(),
        ));
    };
    if !map.contains_key("time") {
        map.insert("time".to_owned(), json!({ "created": time }));
    }
    let mut metadata = match map.remove("metadata") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(existing)) => existing,
        Some(_) => {
            return Err(StoreError::Invalid(
                "inbox payload metadata must be a JSON object".to_owned(),
            ));
        }
    };
    if kind == "peer" {
        metadata
            .entry("source".to_owned())
            .or_insert_with(|| Value::String("peer".to_owned()));
    }
    metadata.insert("inboxID".to_owned(), Value::String(inbox_id.to_owned()));
    metadata.insert("delivered".to_owned(), Value::Bool(false));
    map.insert("metadata".to_owned(), Value::Object(metadata));
    Ok(Value::Object(map))
}

impl Store {
    /// Enqueues pending input. A duplicate id is a `Conflict` error.
    pub async fn enqueue(&self, new: NewInbox) -> Result<Value, StoreError> {
        validate_inbox(&new)?;
        let NewInbox {
            id,
            session_id,
            kind,
            payload,
            delivery,
        } = new;
        let encoded = db::encode_json(&payload)?;
        let time = now_ms();
        let id_for_read = id.clone();
        let session_for_read = session_id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                insert_inbox_row(
                    transaction,
                    &id,
                    &session_id,
                    &kind,
                    &encoded,
                    delivery,
                    time,
                )
            })
        })
        .await?;
        self.inbox_item(&session_for_read, &id_for_read)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("inbox item {id_for_read}")))
    }

    /// Enqueues a `steer` item and materialises its message row atomically.
    ///
    /// [`Self::enqueue`] followed by [`Self::append_message`] is two
    /// transactions. A caller that admits a mid-turn prompt (`POST
    /// /api/session/:sid/prompt` with `delivery: "steer"`) needs both rows or
    /// neither: the inbox row alone would be promoted by the runtime on the
    /// next turn even though the caller was told the prompt failed. This method
    /// commits the inbox row and the message row together, so a failed message
    /// insert leaves no executable row behind.
    ///
    /// The message body is the inbox payload plus `time.created` (the same
    /// contract [`Self::promote_inbox`] applies) and the message type is the
    /// wire-normalized kind: the additive `peer`/`observer` kinds materialise
    /// as `synthetic` exactly like [`Self::promote_inbox`] does, because the
    /// message union has no such type. Only [`Delivery::Steer`] is accepted:
    /// `queue` and control kinds stay inbox-only until the runtime promotes
    /// them, and a materialised `queue` row would leak into the running turn.
    ///
    /// A message id that already exists in another session violates the
    /// database-wide `session_message.id` primary key; the resulting
    /// [`StoreError::Sqlite`] is surfaced unchanged (matching
    /// [`Self::append_message`]) and the transaction rolls back both rows.
    pub async fn enqueue_steer(&self, new: NewInbox) -> Result<Value, StoreError> {
        validate_inbox(&new)?;
        if new.delivery != Delivery::Steer {
            return Err(StoreError::Invalid(
                "materialised admission requires steer delivery".to_owned(),
            ));
        }
        let NewInbox {
            id,
            session_id,
            kind,
            payload,
            delivery,
        } = new;
        // Same normalization `promote_inbox`/`inbox_value` apply: a `peer` or
        // `observer` steer must not materialise an off-union message type. The
        // row itself is stored under the normalized kind, which is idempotent
        // for every other kind.
        let kind = message_kind(&kind).to_owned();
        let time = now_ms();
        let body = message::body_with_time(&payload, time)?;
        let encoded_payload = db::encode_json(&payload)?;
        let encoded_body = db::encode_json(&body)?;
        let item = json!({
            "id": id,
            "sessionID": session_id,
            "type": kind,
            "payload": payload,
            "delivery": delivery.as_str(),
            "time": { "created": time },
        });
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                // Same allocation order as `enqueue` + `append_message`, so a
                // promoted row keeps its admission seq.
                insert_inbox_row(
                    transaction,
                    &id,
                    &session_id,
                    &kind,
                    &encoded_payload,
                    delivery,
                    time,
                )?;
                let message_seq = db::next_seq(transaction, &session_id)?;
                transaction.execute(
                    "INSERT INTO session_message
                        (id, session_id, type, seq, data, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![&id, &session_id, &kind, message_seq, &encoded_body, time],
                )?;
                db::touch_session(transaction, &session_id, time)?;
                Ok(())
            })
        })
        .await?;
        Ok(item)
    }

    /// Enqueues pending input and materialises its transcript message in one
    /// transaction.
    ///
    /// [`Self::enqueue`] leaves a row invisible to every transcript reader
    /// until the runtime promotes it. A peer message that is queued and then
    /// cancelled therefore never becomes a message and disappears from history
    /// entirely, and a queued-then-delivered one only appears when its turn
    /// starts. This method commits the inbox doorbell and the transcript row
    /// together at admission time: the message is visible immediately (with
    /// `metadata.delivered = false`) and a cancellation can keep it with a
    /// cancelled marker instead of deleting the only record.
    ///
    /// The message type is [`message_kind`] of the inbox kind — `peer` and
    /// `observer` materialise as `synthetic` exactly like [`Self::promote_inbox`]
    /// does. The body is the inbox payload plus `time.created` and the merged
    /// `metadata` described on [`materialized_body`]. Both rows are committed
    /// or neither is.
    ///
    /// Control kinds (`move`, `compaction`) have no transcript message and are
    /// rejected. Promotion must follow: [`Self::promote_inbox`] detects
    /// `metadata.inboxID` and flips `delivered` instead of inserting a second
    /// row, so the enqueue-time `seq` is preserved.
    pub async fn enqueue_materialized(
        &self,
        item: &NewInbox,
    ) -> Result<(InboxRow, MessageId), StoreError> {
        validate_inbox(item)?;
        if !materializable(&item.kind) {
            return Err(StoreError::Invalid(format!(
                "inbox type {:?} cannot be materialised",
                item.kind
            )));
        }
        let id = item.id.clone();
        let session_id = item.session_id.clone();
        let kind = message_kind(&item.kind).to_owned();
        let time = now_ms();
        let body = materialized_body(&item.payload, &item.id, &item.kind, time)?;
        let encoded_payload = db::encode_json(&item.payload)?;
        let encoded_body = db::encode_json(&body)?;
        let delivery = item.delivery;
        let stored_kind = item.kind.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                insert_inbox_row(
                    transaction,
                    &id,
                    &session_id,
                    &stored_kind,
                    &encoded_payload,
                    delivery,
                    time,
                )?;
                // Same sequence source as `append_message`, so the row is
                // chronologically ordered where it was admitted.
                let seq = db::next_seq(transaction, &session_id)?;
                transaction.execute(
                    "INSERT INTO session_message
                        (id, session_id, type, seq, data, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![&id, &session_id, &kind, seq, &encoded_body, time],
                )?;
                db::touch_session(transaction, &session_id, time)?;
                Ok(())
            })
        })
        .await?;
        Ok((
            InboxRow {
                id: item.id.clone(),
                session_id: item.session_id.clone(),
                kind: item.kind.clone(),
                payload: item.payload.clone(),
                delivery: item.delivery,
                time_created: time,
            },
            MessageId::from_wire(item.id.clone()),
        ))
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
    ///
    /// A row whose message was already materialised by
    /// [`Self::enqueue_materialized`] is detected through `metadata.inboxID`:
    /// that row is flipped to `metadata.delivered = true` with a
    /// `metadata.deliveredAt` timestamp and returned as-is, so promotion never
    /// inserts a second transcript row and never re-allocates its `seq`. Rows
    /// without the anchor (user prompts, legacy peers) keep the original
    /// insert-at-promotion path.
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
                if !materializable(&kind) {
                    return Ok(None);
                }
                let removed = transaction.execute(
                    "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id],
                )?;
                if removed == 0 {
                    // Not reachable today: the SELECT above and this DELETE run in
                    // one `TransactionBehavior::Immediate` transaction on the single
                    // serialized writer thread (`db::spawn_writer`), no trigger
                    // touches `session_inbox`, the only foreign key that could
                    // cascade (`session_v2`) cannot change inside that transaction,
                    // and a second process is excluded by the database PID lock. It
                    // is kept as a cheap defence should a multi-writer future ever
                    // appear: then the row could already be gone, and inserting
                    // here would materialize a duplicate transcript message.
                    // `None` is the documented "absent item" answer either way.
                    return Ok(None);
                }
                let kind = message_kind(&kind);
                let payload: Value = db::decode_json(&payload)?;
                if let Some(delivered) = mark_delivered_materialized(transaction, &session_id, &id)?
                {
                    db::touch_session(transaction, &session_id, time_created)?;
                    return Ok(Some(delivered));
                }
                let body = message::body_with_time(&payload, time_created)?;
                let encoded = db::encode_json(&body)?;
                let seq = db::next_seq(transaction, &session_id)?;
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
                    kind,
                    time_created,
                )?))
            })
        })
        .await
    }

    /// Cancels one pending item. Returns whether an inbox row was removed.
    ///
    /// A peer message stays in the transcript: when its row was materialised at
    /// admission and belongs to a peer (`metadata.source == "peer"`), the
    /// message is kept and marked `metadata.cancelled = true` with
    /// `metadata.cancelledAt`. Every other kind keeps the delete-only
    /// semantics, and an already-delivered peer row is not rewritten.
    ///
    /// This is the *cancellation* path (user cancel, stop withdrawing queued
    /// input). A caller whose message already exists because it is being
    /// *delivered* must use [`Self::consume_inbox`] instead, or the transcript
    /// would show a cancelled chip for a delivered message.
    pub async fn cancel_inbox(&self, session_id: &str, id: &str) -> Result<bool, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let removed = transaction.execute(
                    "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id],
                )?;
                if removed > 0 && cancel_peer_message(transaction, &session_id, &id, true)? {
                    db::touch_session(transaction, &session_id, now_ms())?;
                }
                Ok(removed > 0)
            })
        })
        .await
    }

    /// Consumes one pending item whose message row already exists. Returns
    /// whether an inbox row was removed.
    ///
    /// The counterpart of [`Self::cancel_inbox`] for the doorbell case: the
    /// caller materialised (or delivered) the message itself and only needs the
    /// pending row retired. Deleting it must not leave a cancelled marker on a
    /// message that is being delivered, so this method never touches
    /// `session_message`.
    pub async fn consume_inbox(&self, session_id: &str, id: &str) -> Result<bool, StoreError> {
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

    /// Marks a peer message row cancelled without deleting it.
    ///
    /// Returns `false` when there is no such row or it is not a peer
    /// (`metadata.source != "peer"`); the row is never deleted by this method.
    /// See [`Self::cancel_inbox`], which applies the same marker to an
    /// undelivered materialised peer row as part of the inbox cancellation.
    pub async fn mark_peer_cancelled(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<bool, StoreError> {
        let session_id = session_id.to_owned();
        let message_id = message_id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let marked = cancel_peer_message(transaction, &session_id, &message_id, false)?;
                if marked {
                    db::touch_session(transaction, &session_id, now_ms())?;
                }
                Ok(marked)
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

/// Flips an already-materialised message to delivered.
///
/// Returns `Some(message)` when the row exists and carries this inbox id in
/// `metadata.inboxID`; `None` keeps the legacy insert path. The row keeps its
/// allocated `seq` and `time.created`.
pub(crate) fn mark_delivered_materialized(
    transaction: &Transaction<'_>,
    session_id: &str,
    id: &str,
) -> Result<Option<Value>, StoreError> {
    let Some((encoded, kind, created)) = message_row(transaction, session_id, id)? else {
        return Ok(None);
    };
    let mut body = db::decode_json(&encoded)?;
    {
        let Value::Object(map) = &mut body else {
            return Ok(None);
        };
        if map
            .get("metadata")
            .and_then(|metadata| metadata.get("inboxID"))
            .and_then(Value::as_str)
            != Some(id)
        {
            return Ok(None);
        }
        let delivered_at = now_ms();
        let Value::Object(metadata) = map
            .entry("metadata".to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
        else {
            return Ok(None);
        };
        metadata.insert("delivered".to_owned(), Value::Bool(true));
        metadata.insert("deliveredAt".to_owned(), json!(delivered_at));
        let encoded = db::encode_json(&body)?;
        transaction.execute(
            "UPDATE session_message
             SET data = ?3, time_updated = max(coalesce(time_updated, 0), ?4)
             WHERE id = ?1 AND session_id = ?2",
            params![id, session_id, encoded, delivered_at],
        )?;
    }
    message::compose(body, id, session_id, &kind, created).map(Some)
}

/// Sets `metadata.cancelled` on a peer message row.
///
/// `only_undelivered` restricts the marker to rows that were never promoted, so
/// cancelling an already-delivered row cannot rewrite consumed history.
pub(crate) fn cancel_peer_message(
    transaction: &Transaction<'_>,
    session_id: &str,
    id: &str,
    only_undelivered: bool,
) -> Result<bool, StoreError> {
    let Some((encoded, _, _)) = message_row(transaction, session_id, id)? else {
        return Ok(false);
    };
    let mut body = db::decode_json(&encoded)?;
    let cancelled_at = now_ms();
    {
        let Value::Object(map) = &mut body else {
            return Ok(false);
        };
        let Some(Value::Object(metadata)) = map.get_mut("metadata") else {
            return Ok(false);
        };
        if metadata.get("source").and_then(Value::as_str) != Some("peer") {
            return Ok(false);
        }
        if metadata.get("cancelled").and_then(Value::as_bool) == Some(true) {
            return Ok(false);
        }
        if only_undelivered && metadata.get("delivered").and_then(Value::as_bool) == Some(true) {
            return Ok(false);
        }
        metadata.insert("cancelled".to_owned(), Value::Bool(true));
        metadata.insert("cancelledAt".to_owned(), json!(cancelled_at));
        let encoded = db::encode_json(&body)?;
        transaction.execute(
            "UPDATE session_message
             SET data = ?3, time_updated = max(coalesce(time_updated, 0), ?4)
             WHERE id = ?1 AND session_id = ?2",
            params![id, session_id, encoded, cancelled_at],
        )?;
    }
    Ok(true)
}

/// Reads one stored message: body, type and creation time.
fn message_row(
    transaction: &Transaction<'_>,
    session_id: &str,
    id: &str,
) -> Result<Option<(String, String, i64)>, StoreError> {
    transaction
        .query_row(
            "SELECT data, type, time_created FROM session_message
             WHERE id = ?1 AND session_id = ?2",
            params![id, session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(StoreError::from)
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
    // Legacy `peer`/`observer` rows are surfaced as the wire-valid kind they
    // promote to; a client union that only knows the upstream four kinds must
    // still render a queued badge for them.
    Ok(json!({
        "id": id,
        "sessionID": session_id,
        "type": message_kind(&kind),
        "payload": payload,
        "delivery": delivery,
        "time": { "created": time_created },
    }))
}
