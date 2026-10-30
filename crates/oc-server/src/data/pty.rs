//! `/api/pty` HTTP routes and the PTY WebSocket attach.
//!
//! The manager is [`oc_pty::PtyManager`]; this module only shapes it for the
//! wire. Every handler answers `503 ServiceUnavailableError` when the manager
//! is not attached rather than pretending a terminal exists.
//!
//! Terminals are scoped to the location recorded at creation: a request that
//! resolves to another directory cannot list, read, update, delete or mint a
//! connect ticket for them (foreign ids answer `404 PtyNotFoundError`). The
//! connect ticket is bound to the terminal's stored location.
//!
//! # HTTP
//!
//! - `GET /api/pty` → location envelope with the `Pty[]` list (exited
//!   terminals stay listed until removed, like upstream).
//! - `POST /api/pty` → `CreatePty` (`command` → shell, `cwd` defaults to the
//!   request location, `size` defaults to 80×24) and `pty.created`.
//! - `GET /api/pty/:id` → one `Pty`, 404 `PtyNotFoundError` when unknown.
//! - `PUT /api/pty/:id` → title (`title`) and/or viewport resize (`size`),
//!   then the updated `Pty`; both fields apply when both are present.
//! - `DELETE /api/pty/:id` → terminate, wait up to three seconds for the exit,
//!   remove, publish `pty.exited` then `pty.deleted`, answer 204.
//! - `POST /api/pty/:id/connect-token` → `{ticket, expires_in: 60}` guarded by
//!   the `x-opencode-ticket: 1` header. Unknown terminals are 404, every other
//!   request is 403.
//!
//! # WebSocket
//!
//! `GET /api/pty/:id/connect?ticket=&cursor=&location[directory]=` bypasses
//! Basic auth because the single-use ticket is the credential. Unknown query
//! parameters (`role`, `attachment_id`, …) are ignored. `cursor` is `-1` for
//! the live tail, a non-negative byte offset, or absent for the oldest retained
//! byte; the manager clamps an evicted cursor to the oldest retained byte.
//!
//! After the ticket is consumed atomically the socket sends the retained
//! backlog as binary frames of at most 64 KiB, then one control frame
//! (`0x00` followed by JSON `{"cursor":<n>}`). Live output follows as binary
//! frames. Inbound text and binary frames are written to the PTY verbatim when
//! they are valid UTF-8; invalid UTF-8 is dropped and nothing is ever passed
//! through a shell. On `Exited` the final retained bytes are flushed and the
//! socket closes with code 4404; the terminal itself stays observable until
//! `DELETE`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::SinkExt;
use futures_util::StreamExt;
use futures_util::stream::SplitSink;
use oc_pty::{CreatePty, PtyError, PtyEvent, PtyInfo, PtyManager};
use oc_types::{ApiError, EventEnvelope, LocationEnvelope};
use rand::Rng as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;

use crate::AppState;
use crate::data::location::{self, RequestLocation};
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error, schema_error};
use crate::error::{ErrorResponse, Respond};

/// Ticket lifetime: upstream mints 60-second single-use tickets.
pub(crate) const TICKET_TTL: Duration = Duration::from_secs(60);
const TICKET_TTL_SECS: u64 = 60;

/// Bounded replay frames, matching the upstream `REPLAY_CHUNK`.
const REPLAY_CHUNK: usize = 64 * 1024;

/// How long `DELETE` waits for the child to exit before removing it anyway.
const EXIT_WAIT: Duration = Duration::from_secs(3);

const TICKET_HEADER: &str = "x-opencode-ticket";

/// One unconsumed WebSocket ticket.
#[derive(Debug)]
struct Ticket {
    pty_id: String,
    directory: String,
    expires: Instant,
}

/// Single-use, location-scoped WebSocket tickets.
///
/// Consumption removes the ticket in the same critical section that checks its
/// scope, so two concurrent attaches cannot both win.
#[derive(Debug, Default)]
pub(crate) struct TicketStore {
    tickets: Mutex<HashMap<String, Ticket>>,
}

impl TicketStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Mint a ticket scoped to `pty_id` and `directory`.
    ///
    /// Returns the opaque token and its lifetime in seconds for the wire.
    pub(crate) fn issue(&self, pty_id: &str, directory: &str) -> (String, u64) {
        let mut bytes = [0_u8; 32];
        rand::rng().fill(&mut bytes);
        let token = hex::encode(bytes);
        let now = Instant::now();
        let mut tickets = lock(&self.tickets);
        tickets.retain(|_, ticket| ticket.expires > now);
        tickets.insert(
            token.clone(),
            Ticket {
                pty_id: pty_id.to_owned(),
                directory: directory.to_owned(),
                expires: now + TICKET_TTL,
            },
        );
        (token, TICKET_TTL_SECS)
    }

    /// Consume `token` when it matches both the terminal and the location.
    ///
    /// A mismatched scope does not burn the ticket: that mirrors upstream's
    /// `Cache.invalidateWhen`, which only invalidates on a match.
    pub(crate) fn consume(&self, token: &str, pty_id: &str, directory: &str) -> bool {
        let now = Instant::now();
        let mut tickets = lock(&self.tickets);
        tickets.retain(|_, ticket| ticket.expires > now);
        let Some(ticket) = tickets.get(token) else {
            return false;
        };
        if ticket.pty_id != pty_id || ticket.directory != directory {
            return false;
        }
        tickets.remove(token);
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Size {
    rows: u16,
    cols: u16,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreatePayload {
    id: Option<String>,
    title: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    cwd: Option<String>,
    env: BTreeMap<String, String>,
    size: Option<Size>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UpdatePayload {
    title: Option<String>,
    size: Option<Size>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConnectParams {
    ticket: Option<String>,
    cursor: Option<String>,
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
}

/// `GET /api/pty`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_pty(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let data: Vec<Value> = manager
        .list()
        .iter()
        .filter(|info| owned_by(info, &resolved))
        .map(info_value)
        .collect();
    Ok(envelope(&resolved, json!(data)))
}

/// `POST /api/pty`
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    body: Result<Json<CreatePayload>, JsonRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let manager = require_pty(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let (cols, rows) = payload.size.map_or((80, 24), |size| (size.cols, size.rows));
    if cols == 0 || rows == 0 {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "pty size rows and cols must be positive",
        )));
    }
    let cwd = payload
        .cwd
        .filter(|value| !value.is_empty())
        .map_or_else(|| resolved.path(), PathBuf::from);
    let info = manager
        .create(CreatePty {
            id: payload.id,
            shell: payload.command,
            args: payload.args,
            cwd,
            location: Some(resolved.path()),
            env: payload.env,
            cols,
            rows,
            title: payload.title,
        })
        .map_err(|error| ErrorResponse::from(map_error(error)))?;
    let value = info_value(&info);
    publish(&state, "pty.created", json!({ "info": value }), &resolved);
    Ok(envelope(&resolved, value))
}

/// `GET /api/pty/:id`
pub async fn get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_pty(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let info = manager
        .info(&id)
        .filter(|info| owned_by(info, &resolved))
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    Ok(envelope(&resolved, info_value(&info)))
}

/// `PUT /api/pty/:id`
pub async fn update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    body: Result<Json<UpdatePayload>, JsonRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let manager = require_pty(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    if !manager
        .info(&id)
        .is_some_and(|info| owned_by(&info, &resolved))
    {
        return Err(ErrorResponse::from(pty_not_found(&id)));
    }

    let mut changed = false;
    if let Some(title) = payload.title {
        manager
            .set_title(&id, &title)
            .map_err(|error| ErrorResponse::from(map_error(error)))?;
        changed = true;
    }

    if let Some(size) = payload.size {
        if size.cols == 0 || size.rows == 0 {
            return Err(ErrorResponse::from(ApiError::new(
                400,
                "SchemaError",
                "pty size rows and cols must be positive",
            )));
        }
        manager
            .resize(&id, size.cols, size.rows)
            .map_err(|error| ErrorResponse::from(map_error(error)))?;
        changed = true;
    }

    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    let value = info_value(&info);
    if changed {
        publish(&state, "pty.updated", json!({ "info": value }), &resolved);
    }
    Ok(envelope(&resolved, value))
}

/// `DELETE /api/pty/:id`
pub async fn remove(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_pty(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let entry = manager
        .info(&id)
        .filter(|info| owned_by(info, &resolved))
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;

    let mut exit = entry.exit;
    if entry.running {
        // Subscribe before signalling so an immediate exit cannot be missed.
        let mut events = manager.subscribe();
        manager
            .terminate(&id)
            .map_err(|error| ErrorResponse::from(map_error(error)))?;
        let observed = timeout(EXIT_WAIT, async {
            loop {
                match events.recv().await {
                    Ok(PtyEvent::Exited { id: event_id, exit }) if event_id == id => return exit,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten();
        exit = observed.or_else(|| manager.info(&id).and_then(|info| info.exit));
    }

    manager
        .remove(&id)
        .map_err(|error| ErrorResponse::from(map_error(error)))?;
    publish(
        &state,
        "pty.exited",
        json!({ "id": id, "exitCode": exit }),
        &resolved,
    );
    publish(&state, "pty.deleted", json!({ "id": id }), &resolved);
    Ok(no_content())
}

/// `POST /api/pty/:id/connect-token`
pub async fn connect_token(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    if headers
        .get(TICKET_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ErrorResponse::from(ApiError::new(
            403,
            "ForbiddenError",
            "Invalid PTY connect token request",
        )));
    }
    let manager = require_pty(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let info = manager
        .info(&id)
        .filter(|info| owned_by(info, &resolved))
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    // The ticket is bound to the terminal's stored location, never the caller's.
    let Some(location) = &info.location else {
        return Err(ErrorResponse::from(pty_not_found(&id)));
    };
    let directory = location.to_string_lossy();
    let (ticket, expires_in) = state.tickets().issue(&id, directory.as_ref());
    Ok(envelope(
        &resolved,
        json!({ "ticket": ticket, "expires_in": expires_in }),
    ))
}

/// `GET /api/pty/:id/connect` — the WebSocket attach.
pub async fn connect(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<ConnectParams>, QueryRejection>,
    ws: WebSocketUpgrade,
) -> Response {
    let Query(params) = match params {
        Ok(params) => params,
        Err(error) => return query_error(error).respond(),
    };
    let Some(manager) = state.pty().cloned() else {
        return ApiError::service_unavailable("pty manager not available").respond();
    };
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let Some(ticket) = params.ticket.as_deref().filter(|ticket| !ticket.is_empty()) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    // Single use, scoped to this terminal and location, consumed before the
    // upgrade completes.
    if !state.tickets().consume(ticket, &id, &resolved.directory) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let cursor = parse_cursor(params.cursor.as_deref());
    ws.on_upgrade(move |socket| attach(manager, id, cursor, socket))
}

/// Parse the `cursor` query: `-1` tails the stream, an offset replays from it,
/// and anything else means "oldest retained".
fn parse_cursor(raw: Option<&str>) -> Option<u64> {
    let raw = raw?;
    if raw == "-1" {
        return Some(u64::MAX);
    }
    raw.parse::<u64>().ok()
}

async fn attach(manager: Arc<PtyManager>, id: String, cursor: Option<u64>, socket: WebSocket) {
    let Some(info) = manager.info(&id) else {
        close_socket(socket, 4404, "session not found").await;
        return;
    };
    if !info.running {
        close_socket(socket, 4404, "session exited").await;
        return;
    }

    // Subscribe before replay: anything produced after the snapshot is either
    // already covered by the replay cursor or delivered as a live event.
    let mut events = manager.subscribe();
    let Ok(replay) = manager.replay(&id, cursor) else {
        close_socket(socket, 4404, "session not found").await;
        return;
    };

    let (mut sink, mut stream) = socket.split();
    if send_bytes(&mut sink, &replay.bytes).await.is_err()
        || send_meta(&mut sink, replay.cursor).await.is_err()
    {
        return;
    }
    let mut sent = replay.cursor;

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(PtyEvent::Output { id: event_id, .. }) if event_id == id => {
                    if !flush(&manager, &id, &mut sink, &mut sent).await {
                        return;
                    }
                }
                Ok(PtyEvent::Exited { id: event_id, .. }) if event_id == id => {
                    // Best effort: `DELETE` may remove the terminal right after
                    // the exit event, in which case the buffer is already gone.
                    let _ = flush(&manager, &id, &mut sink, &mut sent).await;
                    let _ = send_close(&mut sink, 4404, "session exited").await;
                    return;
                }
                Ok(PtyEvent::Output { .. } | PtyEvent::Exited { .. } | PtyEvent::Title { .. }) => {}
                Err(RecvError::Lagged(_)) => {
                    // Slow subscriber: the replay buffer is the source of truth,
                    // so catch up from the last cursor instead of closing.
                    if !flush(&manager, &id, &mut sink, &mut sent).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let _ = manager.write(&id, text.as_str().as_bytes());
                }
                Some(Ok(Message::Binary(data))) => {
                    if let Ok(text) = std::str::from_utf8(&data) {
                        let _ = manager.write(&id, text.as_bytes());
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            }
        }
    }
}

/// Send everything retained after `sent`, advancing it. Output events carry no
/// offset, so the replay buffer (not the event payload) is authoritative; this
/// also drops any bytes already covered by the handshake cursor.
async fn flush(
    manager: &PtyManager,
    id: &str,
    sink: &mut SplitSink<WebSocket, Message>,
    sent: &mut u64,
) -> bool {
    let Ok(output) = manager.replay(id, Some(*sent)) else {
        return true;
    };
    if output.cursor > *sent {
        if send_bytes(sink, &output.bytes).await.is_err() {
            return false;
        }
        *sent = output.cursor;
    }
    true
}

async fn send_bytes(
    sink: &mut SplitSink<WebSocket, Message>,
    bytes: &[u8],
) -> Result<(), axum::Error> {
    for chunk in bytes.chunks(REPLAY_CHUNK) {
        sink.send(Message::Binary(Bytes::copy_from_slice(chunk)))
            .await?;
    }
    Ok(())
}

async fn send_meta(
    sink: &mut SplitSink<WebSocket, Message>,
    cursor: u64,
) -> Result<(), axum::Error> {
    let mut frame = Vec::with_capacity(48);
    frame.push(0);
    frame.extend_from_slice(json!({ "cursor": cursor }).to_string().as_bytes());
    sink.send(Message::Binary(frame.into())).await
}

async fn send_close(
    sink: &mut SplitSink<WebSocket, Message>,
    code: u16,
    reason: &str,
) -> Result<(), axum::Error> {
    sink.send(Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    })))
    .await
}

async fn close_socket(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

fn envelope(location: &RequestLocation, data: Value) -> Json<LocationEnvelope<Value>> {
    Json(LocationEnvelope::new(location.reference(), data))
}

fn publish(state: &AppState, kind: &str, data: Value, location: &RequestLocation) {
    let mut event = EventEnvelope::now(kind, data);
    event.location = Some(location.event());
    state.publish(event);
}

fn require_pty(state: &AppState) -> Result<&Arc<PtyManager>, ApiError> {
    state
        .pty()
        .ok_or_else(|| ApiError::service_unavailable("pty manager not available"))
}

/// Whether `info` belongs to the request's location. A terminal created
/// without a location (only possible through the crate API) is visible to
/// nobody over HTTP.
fn owned_by(info: &PtyInfo, resolved: &RequestLocation) -> bool {
    info.location.as_deref() == Some(Path::new(&resolved.directory))
}

fn pty_not_found(id: &str) -> ApiError {
    ApiError::not_found("PtyNotFoundError", format!("PTY session not found: {id}"))
        .with("ptyID", json!(id))
}

#[allow(clippy::needless_pass_by_value)] // error values are consumed exactly once
fn map_error(error: PtyError) -> ApiError {
    match error {
        PtyError::NotFound { id } => pty_not_found(&id),
        PtyError::Conflict { id } => {
            ApiError::conflict(format!("PTY session already exists: {id}"))
        }
        PtyError::Limit { limit } => {
            ApiError::conflict(format!("live terminal limit reached: {limit}"))
        }
        PtyError::Running { id } => ApiError::conflict(format!("PTY session is running: {id}")),
        PtyError::Exited { id } => ApiError::conflict(format!("PTY session has exited: {id}")),
        PtyError::RowGap { requested, oldest } => ApiError::bad_request(format!(
            "row {requested} is no longer retained (oldest retained row is {oldest})"
        )),
        PtyError::Operation { operation, message } => {
            ApiError::bad_request(format!("pty {operation} failed: {message}"))
        }
        PtyError::Shutdown => ApiError::service_unavailable("pty manager is shut down"),
    }
}

fn info_value(info: &PtyInfo) -> Value {
    let mut value = serde_json::Map::new();
    value.insert("id".to_owned(), json!(info.id));
    value.insert(
        "title".to_owned(),
        json!(info.title.clone().unwrap_or_default()),
    );
    value.insert("command".to_owned(), json!(info.command));
    value.insert("args".to_owned(), json!(info.args));
    value.insert(
        "cwd".to_owned(),
        json!(info.cwd.to_string_lossy().into_owned()),
    );
    value.insert(
        "status".to_owned(),
        json!(if info.running { "running" } else { "exited" }),
    );
    value.insert("pid".to_owned(), json!(info.pid.unwrap_or(0)));
    if let Some(exit) = info.exit.filter(|exit| *exit >= 0) {
        value.insert("exitCode".to_owned(), json!(exit));
    }
    Value::Object(value)
}
