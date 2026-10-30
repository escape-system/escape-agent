//! Durable event log and the synchronous phase-1 projections.
//!
//! `append_event` reserves the dense per-session `seq`, projects the event and
//! inserts the `event` row in a single transaction. Ephemeral events reserve no
//! sequence and persist nothing (they return `0`). Event kinds we do not project
//! still reserve a seq and persist the row; they simply mutate no rows.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};

use crate::db::{self, Store, in_tx};
use crate::error::StoreError;
use crate::inbox::{Delivery, KINDS as INBOX_KINDS};
use crate::message;
use crate::project::ensure_project;
use crate::session::{delete_tree, parent_position};

/// Input for appending a durable event.
#[derive(Debug, Clone)]
pub struct NewEvent {
    /// Event id (`evt_…`).
    pub id: String,
    /// Aggregate (session) the sequence belongs to.
    pub aggregate_id: String,
    /// Event kind without its version (`session.renamed`).
    pub kind: String,
    /// Wire event payload.
    pub data: Value,
    /// `true` for durable events; ephemeral ones reserve no sequence.
    pub durable: bool,
    /// Event version; stored as `<kind>.<version>`.
    pub version: u32,
    /// Creation time in epoch milliseconds.
    pub created: i64,
}

/// A stored event as read back from the log.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    /// Event id.
    pub id: String,
    /// Aggregate (session) id.
    pub aggregate_id: String,
    /// Dense per-aggregate sequence number.
    pub seq: i64,
    /// Event kind without its version.
    pub kind: String,
    /// Event version.
    pub version: u32,
    /// Wire event payload.
    pub data: Value,
    /// Creation time in epoch milliseconds.
    pub created: i64,
}

impl Store {
    /// Appends an event, projects it and returns its sequence number.
    ///
    /// Durable events get the next dense sequence for the aggregate and their
    /// `event` row is persisted; `durable == false` returns `0` and writes
    /// nothing.
    pub async fn append_event(&self, event: NewEvent) -> Result<i64, StoreError> {
        if event.kind.is_empty() {
            return Err(StoreError::Invalid(
                "event kind must not be empty".to_owned(),
            ));
        }
        if event.version == 0 {
            return Err(StoreError::Invalid(
                "event version must be at least 1".to_owned(),
            ));
        }
        if !event.durable {
            return Ok(0);
        }
        let NewEvent {
            id,
            aggregate_id,
            kind,
            data,
            version,
            created,
            ..
        } = event;
        let stored_type = format!("{kind}.{version}");
        let encoded = db::encode_json(&data)?;
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let existing: Option<i64> = transaction
                    .query_row("SELECT 1 FROM event WHERE id = ?1", [&id], |row| row.get(0))
                    .optional()?;
                if existing.is_some() {
                    return Err(StoreError::Conflict(format!("event {id} already exists")));
                }
                let seq = db::next_seq(transaction, &aggregate_id)?;
                transaction.execute(
                    "INSERT INTO event (id, aggregate_id, seq, created, type, data)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![&id, &aggregate_id, seq, created, &stored_type, &encoded],
                )?;
                project(transaction, &id, &kind, seq, created, &data)?;
                Ok(seq)
            })
        })
        .await
    }

    /// Reads durable events after `after`, oldest first.
    pub async fn events_after(
        &self,
        session_id: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let limit = if limit == 0 { 200 } else { limit.min(1_000) };
        let aggregate = session_id.to_owned();
        self.read(move |connection| {
            let mut statement = connection.prepare(
                "SELECT id, aggregate_id, seq, created, type, data
                 FROM event WHERE aggregate_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
            )?;
            let mut rows = statement.query(params![
                &aggregate,
                after,
                i64::try_from(limit).unwrap_or(i64::MAX)
            ])?;
            let mut events = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let aggregate_id: String = row.get(1)?;
                let seq: i64 = row.get(2)?;
                let created: i64 = row.get(3)?;
                let stored_type: String = row.get(4)?;
                let data: String = row.get(5)?;
                let (kind, version) = split_type(&stored_type, &id)?;
                events.push(StoredEvent {
                    id,
                    aggregate_id,
                    seq,
                    kind,
                    version,
                    data: db::decode_json(&data)?,
                    created,
                });
            }
            Ok(events)
        })
        .await
    }
}

fn split_type(stored_type: &str, event_id: &str) -> Result<(String, u32), StoreError> {
    let Some((kind, version)) = stored_type.rsplit_once('.') else {
        return Err(StoreError::Corrupt(format!(
            "event {event_id} type {stored_type:?} has no version"
        )));
    };
    let version = version.parse::<u32>().map_err(|_| {
        StoreError::Corrupt(format!(
            "event {event_id} version {version:?} is not a number"
        ))
    })?;
    Ok((kind.to_owned(), version))
}

fn project(
    transaction: &Transaction<'_>,
    event_id: &str,
    kind: &str,
    seq: i64,
    created: i64,
    data: &Value,
) -> Result<(), StoreError> {
    match kind {
        "session.created" => project_created(transaction, data, created),
        "session.renamed" => update_text(transaction, data, created, "title", "title"),
        "session.agent.selected" => update_text(transaction, data, created, "agent", "agent"),
        "session.model.selected" => update_json(transaction, data, created, "model", "model"),
        "session.metadata.updated" => {
            update_json(transaction, data, created, "metadata", "metadata")
        }
        "session.permissions" => {
            update_json(transaction, data, created, "permission", "permissions")
        }
        "session.moved" => project_moved(transaction, data, created),
        "session.viewed" => project_viewed(transaction, data),
        "session.archived" => project_archived(transaction, data, created, true),
        "session.unarchived" => project_archived(transaction, data, created, false),
        "session.deleted" => project_deleted(transaction, data),
        "session.inbox.enqueued" => project_inbox_enqueued(transaction, data, seq, created),
        "session.inbox.delivered" => project_inbox_delivered(transaction, data, seq, created),
        "session.inbox.cancelled" => project_inbox_cancelled(transaction, data),
        "session.inbox.delivery.changed" => project_delivery_changed(transaction, data),
        "session.synthetic" => project_synthetic(transaction, event_id, data, seq, created),
        "session.message.content.updated" => project_content_updated(transaction, data, created),
        _ => Ok(()),
    }
}

fn session_of(data: &Value) -> Option<&str> {
    data.get("sessionID").and_then(Value::as_str)
}

fn field<'a>(data: &'a Value, name: &str) -> Option<&'a str> {
    data.get(name).and_then(Value::as_str)
}

fn project_created(
    transaction: &Transaction<'_>,
    data: &Value,
    created: i64,
) -> Result<(), StoreError> {
    let Some(session_id) = field(data, "sessionID") else {
        return Ok(());
    };
    let Some(project_id) = field(data, "projectID") else {
        return Ok(());
    };
    let directory = data
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .or_else(|| field(data, "directory"));
    ensure_project(
        transaction,
        project_id,
        directory.unwrap_or_default(),
        created,
    )?;

    let parent_id = field(data, "parentID");
    let (root_id, depth) = match parent_id {
        Some(parent) => match parent_position(transaction, parent)? {
            Some((root_id, depth)) => (root_id.unwrap_or_else(|| parent.to_owned()), depth + 1),
            None => (session_id.to_owned(), 0),
        },
        None => (session_id.to_owned(), 0),
    };
    let model = db::encode_json_opt(data.get("model"))?;
    let metadata = db::encode_json_opt(data.get("metadata"))?;
    let permission = db::encode_json_opt(data.get("permissions"))?;

    transaction.execute(
        "INSERT OR IGNORE INTO session_v2
            (id, project_id, parent_id, root_id, depth, directory, path, title, agent,
             model, metadata, permission, time_created, time_updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            session_id,
            project_id,
            parent_id,
            root_id,
            depth,
            directory,
            field(data, "subpath"),
            field(data, "title"),
            field(data, "agent"),
            model.as_deref(),
            metadata.as_deref(),
            permission.as_deref(),
            created,
        ],
    )?;
    if transaction.changes() > 0
        && let Some(parent) = parent_id
    {
        transaction.execute(
            "UPDATE session_v2 SET child_count = child_count + 1 WHERE id = ?1",
            [parent],
        )?;
    }
    Ok(())
}

fn update_text(
    transaction: &Transaction<'_>,
    data: &Value,
    created: i64,
    column: &str,
    property: &str,
) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    let Some(value) = field(data, property) else {
        return Ok(());
    };
    transaction.execute(
        &format!(
            "UPDATE session_v2 SET {column} = ?2, time_updated = max(coalesce(time_updated, 0), ?3)
             WHERE id = ?1"
        ),
        params![session_id, value, created],
    )?;
    Ok(())
}

fn update_json(
    transaction: &Transaction<'_>,
    data: &Value,
    created: i64,
    column: &str,
    property: &str,
) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    let encoded = db::encode_json_opt(data.get(property))?;
    transaction.execute(
        &format!(
            "UPDATE session_v2 SET {column} = ?2, time_updated = max(coalesce(time_updated, 0), ?3)
             WHERE id = ?1"
        ),
        params![session_id, encoded, created],
    )?;
    Ok(())
}

fn project_moved(
    transaction: &Transaction<'_>,
    data: &Value,
    created: i64,
) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    let directory = data
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str);
    let project_id = field(data, "projectID");
    if let Some(project_id) = project_id {
        ensure_project(
            transaction,
            project_id,
            directory.unwrap_or_default(),
            created,
        )?;
    }
    transaction.execute(
        "UPDATE session_v2
         SET directory = coalesce(?2, directory),
             project_id = coalesce(?3, project_id),
             path = ?4,
             time_updated = max(coalesce(time_updated, 0), ?5)
         WHERE id = ?1",
        params![
            session_id,
            directory,
            project_id,
            field(data, "subpath"),
            created
        ],
    )?;
    Ok(())
}

fn project_viewed(transaction: &Transaction<'_>, data: &Value) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    let Some(idle) = data.get("idle").and_then(Value::as_i64) else {
        return Ok(());
    };
    transaction.execute(
        "UPDATE session_v2 SET time_viewed = max(coalesce(time_viewed, ?2), ?2) WHERE id = ?1",
        params![session_id, idle],
    )?;
    Ok(())
}

fn project_archived(
    transaction: &Transaction<'_>,
    data: &Value,
    created: i64,
    archived: bool,
) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    if archived {
        transaction.execute(
            "UPDATE session_v2
             SET time_archived = ?2, time_updated = max(coalesce(time_updated, 0), ?2)
             WHERE id = ?1",
            params![session_id, created],
        )?;
    } else {
        transaction.execute(
            "UPDATE session_v2 SET time_archived = NULL, time_updated = ?2 WHERE id = ?1",
            params![session_id, created],
        )?;
    }
    Ok(())
}

fn project_deleted(transaction: &Transaction<'_>, data: &Value) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    delete_tree(transaction, session_id)?;
    Ok(())
}

fn project_inbox_enqueued(
    transaction: &Transaction<'_>,
    data: &Value,
    seq: i64,
    created: i64,
) -> Result<(), StoreError> {
    let (Some(session_id), Some(inbox_id), Some(item)) =
        (session_of(data), field(data, "inboxID"), data.get("item"))
    else {
        return Ok(());
    };
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if !INBOX_KINDS.contains(&kind) {
        return Err(StoreError::Invalid(format!("unknown inbox type {kind:?}")));
    }
    let delivery = Delivery::parse(
        item.get("delivery")
            .and_then(Value::as_str)
            .unwrap_or("queue"),
    )?;
    let payload = item
        .get("payload")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let encoded = db::encode_json(&payload)?;
    transaction.execute(
        "INSERT OR IGNORE INTO session_inbox
            (id, session_id, type, payload, delivery, enqueued_seq, time_created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            inbox_id,
            session_id,
            kind,
            encoded,
            delivery.as_str(),
            seq,
            created
        ],
    )?;
    db::touch_session(transaction, session_id, created)?;
    Ok(())
}

fn project_inbox_delivered(
    transaction: &Transaction<'_>,
    data: &Value,
    seq: i64,
    created: i64,
) -> Result<(), StoreError> {
    let (Some(session_id), Some(inbox_id)) = (session_of(data), field(data, "inboxID")) else {
        return Ok(());
    };
    let row: Option<(String, String)> = transaction
        .query_row(
            "SELECT type, payload FROM session_inbox WHERE id = ?1 AND session_id = ?2",
            params![inbox_id, session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((kind, payload)) = row else {
        return Ok(());
    };
    transaction.execute(
        "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2",
        params![inbox_id, session_id],
    )?;
    if kind != "move" && kind != "compaction" {
        let payload: Value = db::decode_json(&payload)?;
        let body = message::body_with_time(&payload, created)?;
        let encoded = db::encode_json(&body)?;
        transaction.execute(
            "INSERT OR IGNORE INTO session_message
                (id, session_id, type, seq, data, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![inbox_id, session_id, kind, seq, encoded, created],
        )?;
    }
    db::touch_session(transaction, session_id, created)?;
    Ok(())
}

fn project_inbox_cancelled(transaction: &Transaction<'_>, data: &Value) -> Result<(), StoreError> {
    let (Some(session_id), Some(inbox_id)) = (session_of(data), field(data, "inboxID")) else {
        return Ok(());
    };
    transaction.execute(
        "DELETE FROM session_inbox WHERE id = ?1 AND session_id = ?2",
        params![inbox_id, session_id],
    )?;
    Ok(())
}

fn project_delivery_changed(transaction: &Transaction<'_>, data: &Value) -> Result<(), StoreError> {
    let (Some(session_id), Some(inbox_id), Some(delivery)) = (
        session_of(data),
        field(data, "inboxID"),
        field(data, "delivery"),
    ) else {
        return Ok(());
    };
    let delivery = Delivery::parse(delivery)?;
    transaction.execute(
        "UPDATE session_inbox SET delivery = ?3 WHERE id = ?1 AND session_id = ?2",
        params![inbox_id, session_id, delivery.as_str()],
    )?;
    Ok(())
}

fn project_synthetic(
    transaction: &Transaction<'_>,
    event_id: &str,
    data: &Value,
    seq: i64,
    created: i64,
) -> Result<(), StoreError> {
    let Some(session_id) = session_of(data) else {
        return Ok(());
    };
    let Value::Object(mut body) = data.clone() else {
        return Ok(());
    };
    body.remove("sessionID");
    if !body.contains_key("time") {
        body.insert("time".to_owned(), json!({ "created": created }));
    }
    let encoded = db::encode_json(&Value::Object(body))?;
    let message_id = message_id_from_event(event_id);
    transaction.execute(
        "INSERT OR IGNORE INTO session_message
            (id, session_id, type, seq, data, time_created, time_updated)
         VALUES (?1, ?2, 'synthetic', ?3, ?4, ?5, ?5)",
        params![message_id, session_id, seq, encoded, created],
    )?;
    db::touch_session(transaction, session_id, created)?;
    Ok(())
}

fn project_content_updated(
    transaction: &Transaction<'_>,
    data: &Value,
    created: i64,
) -> Result<(), StoreError> {
    let (Some(session_id), Some(message_id), Some(content)) = (
        session_of(data),
        field(data, "messageID"),
        data.get("content"),
    ) else {
        return Ok(());
    };
    let existing: Option<String> = transaction
        .query_row(
            "SELECT data FROM session_message WHERE id = ?1 AND session_id = ?2",
            params![message_id, session_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(existing) = existing else {
        return Ok(());
    };
    let mut value = db::decode_json(&existing)?;
    if let Value::Object(map) = &mut value {
        map.insert("content".to_owned(), content.clone());
    }
    let encoded = db::encode_json(&value)?;
    transaction.execute(
        "UPDATE session_message
         SET data = ?3, time_updated = max(coalesce(time_updated, 0), ?4)
         WHERE id = ?1 AND session_id = ?2",
        params![message_id, session_id, encoded, created],
    )?;
    Ok(())
}

fn message_id_from_event(event_id: &str) -> String {
    format!("msg_{}", event_id.strip_prefix("evt_").unwrap_or(event_id))
}
