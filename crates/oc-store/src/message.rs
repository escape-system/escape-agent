//! Message rows (`session_message`) and keyset pagination over `seq`.

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cursor::{self, Cursor, Direction, Order};
use crate::db::{self, DEFAULT_LIMIT, MAX_LIMIT, Store, in_tx};
use crate::error::StoreError;
use crate::session::ensure_session;

/// Input for appending a message row.
#[derive(Debug, Clone)]
pub struct NewMessage {
    /// Wire message id (`msg_…`).
    pub id: String,
    /// Owning session.
    pub session_id: String,
    /// Message discriminator (`user`, `assistant`, `synthetic`, …).
    pub kind: String,
    /// Wire message body; `id`/`sessionID`/`type` are supplied by the store.
    pub data: Value,
    /// Creation time in epoch milliseconds.
    pub time_created: i64,
}

/// Filters and pagination for [`Store::list_messages`].
#[derive(Debug, Clone)]
pub struct MessageQuery {
    /// Session whose transcript is read.
    pub session_id: String,
    /// Page size; `0` means the default (50) and values above 200 are clamped.
    pub limit: usize,
    /// Sort order on `seq`.
    pub order: Order,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    /// Restrict to one message type.
    pub kind: Option<String>,
}

impl MessageQuery {
    /// A query for one session with the default page size and order.
    #[must_use]
    pub fn new(session_id: impl AsRef<str>) -> Self {
        Self {
            session_id: session_id.as_ref().to_owned(),
            limit: DEFAULT_LIMIT,
            order: Order::Desc,
            cursor: None,
            kind: None,
        }
    }
}

/// One page of messages.
#[derive(Debug, Clone)]
pub struct MessagePage {
    /// Wire message objects in the requested order.
    pub items: Vec<Value>,
    /// Keyset cursors for the neighbouring pages.
    pub cursor: Cursor,
}

pub(crate) const MESSAGE_COLUMNS: &str =
    "id, session_id, type, data, time_created, time_updated, seq";

#[derive(Debug, Serialize, Deserialize)]
struct MessageCursor {
    id: String,
    order: Order,
    direction: Direction,
}

impl Store {
    /// Appends a message and allocates its dense per-session `seq`.
    pub async fn append_message(&self, new: NewMessage) -> Result<Value, StoreError> {
        let NewMessage {
            id,
            session_id,
            kind,
            data,
            time_created,
        } = new;
        if !data.is_object() {
            return Err(StoreError::Invalid(
                "message data must be a JSON object".to_owned(),
            ));
        }
        if kind.is_empty() {
            return Err(StoreError::Invalid(
                "message type must not be empty".to_owned(),
            ));
        }
        let encoded = db::encode_json(&data)?;
        let id_for_read = id.clone();
        let session_for_read = session_id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                let existing: Option<i64> = transaction
                    .query_row(
                        "SELECT 1 FROM session_message WHERE id = ?1 AND session_id = ?2",
                        params![&id, &session_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if existing.is_some() {
                    return Err(StoreError::Conflict(format!("message {id} already exists")));
                }
                let seq = db::next_seq(transaction, &session_id)?;
                transaction.execute(
                    "INSERT INTO session_message
                        (id, session_id, type, seq, data, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    params![&id, &session_id, &kind, seq, &encoded, time_created],
                )?;
                db::touch_session(transaction, &session_id, time_created)?;
                Ok(())
            })
        })
        .await?;
        self.get_message(&session_for_read, &id_for_read)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("message {id_for_read}")))
    }

    /// Loads one message as a wire message object.
    pub async fn get_message(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<Option<Value>, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {MESSAGE_COLUMNS} FROM session_message WHERE id = ?1 AND session_id = ?2"
            );
            let row = connection
                .query_row(&sql, params![&id, &session_id], |row| {
                    Ok(message_value(row))
                })
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Replaces a message body. The allocated `seq` never changes.
    pub async fn update_message(
        &self,
        session_id: &str,
        id: &str,
        data: Value,
    ) -> Result<(), StoreError> {
        if !data.is_object() {
            return Err(StoreError::Invalid(
                "message data must be a JSON object".to_owned(),
            ));
        }
        let encoded = db::encode_json(&data)?;
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        let updated = db::now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE session_message SET data = ?3, time_updated = ?4
                     WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id, &encoded, updated],
                )?;
                if changed == 0 {
                    return Err(StoreError::NotFound(format!("message {id}")));
                }
                db::touch_session(transaction, &session_id, updated)?;
                Ok(())
            })
        })
        .await
    }

    /// Lists messages with keyset pagination on `seq`.
    #[allow(clippy::too_many_lines)] // one linear keyset query; splitting hides the cursor contract
    pub async fn list_messages(&self, query: MessageQuery) -> Result<MessagePage, StoreError> {
        let limit = if query.limit == 0 {
            DEFAULT_LIMIT
        } else {
            query.limit.min(MAX_LIMIT)
        };
        let (anchor_id, direction, order) = match query.cursor.as_deref() {
            Some(raw) => {
                let decoded: MessageCursor = cursor::decode(raw)?;
                (Some(decoded.id), decoded.direction, decoded.order)
            }
            None => (None, Direction::Next, query.order),
        };
        let effective = if direction == Direction::Previous {
            order.flipped()
        } else {
            order
        };

        let anchor_seq = match &anchor_id {
            Some(anchor_id) => {
                let session_id = query.session_id.clone();
                let anchor_id = anchor_id.clone();
                self.read(move |connection| {
                    let seq: Option<i64> = connection
                        .query_row(
                            "SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2",
                            params![&session_id, &anchor_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    Ok(seq)
                })
                .await?
            }
            None => None,
        };
        if anchor_id.is_some() && anchor_seq.is_none() {
            return Ok(MessagePage {
                items: Vec::new(),
                cursor: Cursor::default(),
            });
        }

        let mut conditions = vec!["session_id = ?".to_owned()];
        let mut values: Vec<SqlValue> = vec![SqlValue::Text(query.session_id.clone())];
        if let Some(kind) = &query.kind {
            conditions.push("type = ?".to_owned());
            values.push(SqlValue::Text(kind.clone()));
        }
        if let Some(seq) = anchor_seq {
            let condition = if effective == Order::Asc {
                "seq > ?"
            } else {
                "seq < ?"
            };
            conditions.push(condition.to_owned());
            values.push(SqlValue::Integer(seq));
        }
        values.push(SqlValue::Integer(
            i64::try_from(limit + 1).unwrap_or(i64::MAX),
        ));

        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM session_message WHERE {} ORDER BY seq {} LIMIT ?",
            conditions.join(" AND "),
            effective.sql()
        );
        let rows = self
            .read(move |connection| {
                let mut statement = connection.prepare(&sql)?;
                let mut mapped = statement.query(params_from_iter(values.iter()))?;
                let mut items: Vec<(String, Value)> = Vec::new();
                while let Some(row) = mapped.next()? {
                    let id: String = row.get(0)?;
                    items.push((id, message_value(row)?));
                }
                Ok(items)
            })
            .await?;

        let has_more = rows.len() > limit;
        let mut rows = rows;
        rows.truncate(limit);
        if direction == Direction::Previous {
            rows.reverse();
        }

        let encode = |id: &str, direction: Direction| {
            cursor::encode(&MessageCursor {
                id: id.to_owned(),
                order,
                direction,
            })
        };
        let mut page_cursor = Cursor::default();
        if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
            let next = match direction {
                Direction::Next => {
                    if has_more {
                        Some(encode(&last.0, Direction::Next)?)
                    } else {
                        None
                    }
                }
                Direction::Previous => Some(encode(&last.0, Direction::Next)?),
            };
            let previous = match direction {
                Direction::Next => {
                    if anchor_id.is_some() {
                        Some(encode(&first.0, Direction::Previous)?)
                    } else {
                        None
                    }
                }
                Direction::Previous => {
                    if has_more {
                        Some(encode(&first.0, Direction::Previous)?)
                    } else {
                        None
                    }
                }
            };
            page_cursor = Cursor { previous, next };
        }

        Ok(MessagePage {
            items: rows.into_iter().map(|(_, value)| value).collect(),
            cursor: page_cursor,
        })
    }
}

fn message_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let data: String = row.get(3)?;
    let time_created: i64 = row.get(4)?;
    compose(
        db::decode_json(&data)?,
        &id,
        &session_id,
        &kind,
        time_created,
    )
}

/// Adds the identity fields a wire message carries on top of its stored body.
pub(crate) fn compose(
    data: Value,
    id: &str,
    session_id: &str,
    kind: &str,
    time_created: i64,
) -> Result<Value, StoreError> {
    let Value::Object(mut map) = data else {
        return Err(StoreError::Corrupt(format!(
            "message {id} body is not a JSON object"
        )));
    };
    map.insert("id".to_owned(), Value::String(id.to_owned()));
    map.insert("sessionID".to_owned(), Value::String(session_id.to_owned()));
    map.insert("type".to_owned(), Value::String(kind.to_owned()));
    if !map.contains_key("time") {
        map.insert("time".to_owned(), json!({ "created": time_created }));
    }
    Ok(Value::Object(map))
}

/// Adds a `time.created` to an inbox payload so it can be promoted to a message.
pub(crate) fn body_with_time(payload: &Value, time_created: i64) -> Result<Value, StoreError> {
    let Value::Object(mut map) = payload.clone() else {
        return Err(StoreError::Invalid(
            "inbox payload must be a JSON object".to_owned(),
        ));
    };
    if !map.contains_key("time") {
        map.insert("time".to_owned(), json!({ "created": time_created }));
    }
    Ok(Value::Object(map))
}
