//! Durable lane ticket history (`lane_ticket`).
//!
//! The lane service keeps live tickets in memory; the store mirrors each
//! transition so the UI can show what ran after a restart. Bodies are the wire
//! `oc_lanes::Ticket` JSON shape, but the store stays independent of the lane
//! crate: it reads and writes plain [`Value`]s.
//!
//! Wire shape (camelCase):
//! `{id, lane, command, cwd, priority, ownerSession?, state, queuedMs,
//! startedMs?, finishedMs?, outputPath?}` where `state` is the internally
//! tagged enum object `{"state":"queued"}`, `{"state":"running"}`,
//! `{"state":"cancelled"}`, `{"state":"succeeded","exit":0}` or
//! `{"state":"failed","exit":N,"message":"…"}`.

use rusqlite::{Row, params};
use serde_json::{Map, Value, json};

use crate::db::{DEFAULT_LIMIT, MAX_LIMIT, Store, in_tx};
use crate::error::StoreError;

const TICKET_COLUMNS: &str = "id, lane, command, cwd, priority, owner_session, state, \
queued, started, finished, exit_code, message, output_path";

/// A wire ticket decomposed into the columns the table stores.
struct StoredTicket {
    id: String,
    lane: String,
    command: String,
    cwd: String,
    priority: i64,
    owner_session: Option<String>,
    status: String,
    queued: i64,
    started: Option<i64>,
    finished: Option<i64>,
    exit_code: Option<i64>,
    message: Option<String>,
    output_path: Option<String>,
}

impl Store {
    /// Inserts or replaces one ticket row from its wire `Ticket` JSON.
    ///
    /// Unknown or incomplete payloads are [`StoreError::Invalid`]; a ticket id
    /// that already exists is updated in place (each service transition saves
    /// the whole ticket).
    pub async fn save_lane_ticket(&self, ticket: &Value) -> Result<(), StoreError> {
        let ticket = StoredTicket::parse(ticket)?;
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                transaction.execute(
                    "INSERT INTO lane_ticket
                        (id, lane, command, cwd, priority, owner_session, state,
                         queued, started, finished, exit_code, message, output_path)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                     ON CONFLICT(id) DO UPDATE SET
                        lane = excluded.lane,
                        command = excluded.command,
                        cwd = excluded.cwd,
                        priority = excluded.priority,
                        owner_session = excluded.owner_session,
                        state = excluded.state,
                        queued = excluded.queued,
                        started = excluded.started,
                        finished = excluded.finished,
                        exit_code = excluded.exit_code,
                        message = excluded.message,
                        output_path = excluded.output_path",
                    params![
                        &ticket.id,
                        &ticket.lane,
                        &ticket.command,
                        &ticket.cwd,
                        ticket.priority,
                        ticket.owner_session.as_deref(),
                        &ticket.status,
                        ticket.queued,
                        ticket.started,
                        ticket.finished,
                        ticket.exit_code,
                        ticket.message.as_deref(),
                        ticket.output_path.as_deref(),
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    /// Lists ticket history newest first. `limit = 0` uses the default page
    /// size and values above the maximum are clamped.
    pub async fn lane_tickets(&self, limit: usize) -> Result<Vec<Value>, StoreError> {
        let limit = if limit == 0 {
            DEFAULT_LIMIT
        } else {
            limit.min(MAX_LIMIT)
        };
        let limit = i64::try_from(limit).unwrap_or(i64::from(i32::MAX));
        self.read(move |connection| {
            let sql = format!(
                "SELECT {TICKET_COLUMNS} FROM lane_ticket
                 ORDER BY queued DESC, id DESC LIMIT ?1"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([limit])?;
            let mut tickets = Vec::new();
            while let Some(row) = rows.next()? {
                tickets.push(ticket_value(row)?);
            }
            Ok(tickets)
        })
        .await
    }
}

impl StoredTicket {
    fn parse(value: &Value) -> Result<Self, StoreError> {
        let id = required_str(value, "id")?;
        let lane = required_str(value, "lane")?;
        let command = required_str(value, "command")?;
        let cwd = required_str(value, "cwd")?;
        let priority = required_i64(value, "priority")?;
        let queued = required_i64(value, "queuedMs")?;
        let owner_session = optional_str(value, "ownerSession")?;
        let started = optional_i64(value, "startedMs")?;
        let finished = optional_i64(value, "finishedMs")?;
        let output_path = optional_str(value, "outputPath")?;
        let (status, exit_code, message) = parse_state(value.get("state"))?;
        Ok(Self {
            id,
            lane,
            command,
            cwd,
            priority,
            owner_session,
            status,
            queued,
            started,
            finished,
            exit_code,
            message,
            output_path,
        })
    }
}

/// Splits the internally tagged `TicketState` into columns.
fn parse_state(state: Option<&Value>) -> Result<(String, Option<i64>, Option<String>), StoreError> {
    let state = state.ok_or_else(|| StoreError::Invalid("lane ticket has no state".to_owned()))?;
    // String states are accepted for older internal callers; the wire always
    // sends an object.
    let (status, object) = match state {
        Value::String(status) => (status.as_str(), None),
        Value::Object(object) => {
            let status = object
                .get("state")
                .and_then(Value::as_str)
                .ok_or_else(|| StoreError::Invalid("lane ticket state has no name".to_owned()))?;
            (status, Some(object))
        }
        _ => {
            return Err(StoreError::Invalid(
                "lane ticket state must be a string or object".to_owned(),
            ));
        }
    };
    let exit_code = object
        .and_then(|object| object.get("exit"))
        .map(|exit| {
            exit.as_i64()
                .ok_or_else(|| StoreError::Invalid("lane ticket exit is not an integer".to_owned()))
        })
        .transpose()?;
    let message = object
        .and_then(|object| object.get("message"))
        .map(|message| {
            message.as_str().map(str::to_owned).ok_or_else(|| {
                StoreError::Invalid("lane ticket message is not a string".to_owned())
            })
        })
        .transpose()?;
    let status = match status {
        "queued" | "running" | "cancelled" | "succeeded" | "failed" => status.to_owned(),
        other => {
            return Err(StoreError::Invalid(format!(
                "unknown lane ticket state {other:?}"
            )));
        }
    };
    Ok((status, exit_code, message))
}

fn required_str(value: &Value, field: &str) -> Result<String, StoreError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| StoreError::Invalid(format!("lane ticket {field} must be a string")))
}

fn optional_str(value: &Value, field: &str) -> Result<Option<String>, StoreError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(StoreError::Invalid(format!(
            "lane ticket {field} must be a string"
        ))),
    }
}

fn required_i64(value: &Value, field: &str) -> Result<i64, StoreError> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| StoreError::Invalid(format!("lane ticket {field} must be an integer")))
}

fn optional_i64(value: &Value, field: &str) -> Result<Option<i64>, StoreError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(number) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| StoreError::Invalid(format!("lane ticket {field} must be an integer"))),
    }
}

/// Rebuilds the wire `Ticket` from one row; optional fields are omitted.
fn ticket_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let lane: String = row.get(1)?;
    let command: String = row.get(2)?;
    let cwd: String = row.get(3)?;
    let priority: i64 = row.get(4)?;
    let owner_session: Option<String> = row.get(5)?;
    let status: String = row.get(6)?;
    let queued: i64 = row.get(7)?;
    let started: Option<i64> = row.get(8)?;
    let finished: Option<i64> = row.get(9)?;
    let exit_code: Option<i64> = row.get(10)?;
    let message: Option<String> = row.get(11)?;
    let output_path: Option<String> = row.get(12)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("lane".to_owned(), Value::String(lane));
    map.insert("command".to_owned(), Value::String(command));
    map.insert("cwd".to_owned(), Value::String(cwd));
    map.insert("priority".to_owned(), json!(priority));
    if let Some(owner_session) = owner_session {
        map.insert("ownerSession".to_owned(), Value::String(owner_session));
    }
    let mut state = Map::new();
    state.insert("state".to_owned(), Value::String(status.clone()));
    match status.as_str() {
        "succeeded" => {
            state.insert("exit".to_owned(), json!(exit_code.unwrap_or(0)));
        }
        "failed" => {
            state.insert("exit".to_owned(), json!(exit_code.unwrap_or(0)));
            state.insert(
                "message".to_owned(),
                Value::String(message.unwrap_or_default()),
            );
        }
        "queued" | "running" | "cancelled" => {}
        other => {
            return Err(StoreError::Corrupt(format!(
                "lane ticket has unknown state {other:?}"
            )));
        }
    }
    map.insert("state".to_owned(), Value::Object(state));
    map.insert("queuedMs".to_owned(), json!(queued));
    if let Some(started) = started {
        map.insert("startedMs".to_owned(), json!(started));
    }
    if let Some(finished) = finished {
        map.insert("finishedMs".to_owned(), json!(finished));
    }
    if let Some(output_path) = output_path {
        map.insert("outputPath".to_owned(), Value::String(output_path));
    }
    Ok(Value::Object(map))
}
