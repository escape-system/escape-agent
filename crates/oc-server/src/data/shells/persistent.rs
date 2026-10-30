//! Persistent-PTY aliases over [`oc_pty::PtyManager`], bound to a session id.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use oc_pty::{CreatePty, PtyError, PtyInfo, PtyManager};
use oc_types::{ApiError, DataEnvelope, EventEnvelope, EventLocation};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::AppState;
use crate::data::location;
use crate::data::{no_content, query_error, schema_error};
use crate::error::ErrorResponse;

use super::util::lock;
use super::{EXIT_WAIT, TICKET_HEADER};

// ---------------------------------------------------------------------------
// Persistent PTY aliases
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Binding {
    session_id: String,
}

static BINDINGS: LazyLock<Mutex<HashMap<String, Binding>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CURRENT: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PersistentCreate {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    command: Option<String>,
    args: Vec<String>,
    cwd: Option<String>,
    title: String,
    env: BTreeMap<String, String>,
    size: Option<PtySize>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct PtySize {
    cols: u16,
    rows: u16,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PersistentUpdate {
    size: Option<PtySize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ReadParams {
    lines: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PersistentListParams {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// `GET /api/experimental/persistent-pty`
#[allow(clippy::needless_collect)] // the collect releases the bindings lock
pub(crate) async fn persistent_list(
    State(state): State<AppState>,
    params: Result<Query<PersistentListParams>, QueryRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_pty(&state)?;
    let bindings: Vec<(String, String)> = lock(&BINDINGS)
        .iter()
        .filter(|(_, binding)| {
            params
                .session_id
                .as_deref()
                .is_none_or(|session| binding.session_id == session)
        })
        .map(|(id, binding)| ((*id).clone(), binding.session_id.clone()))
        .collect();
    let mut terminals = Vec::new();
    for (id, session_id) in bindings {
        if let Some(info) = manager.info(&id) {
            terminals.push(persistent_info(&info, &session_id));
        }
    }
    Ok(Json(DataEnvelope::new(Value::Array(terminals))))
}

/// `POST /api/experimental/persistent-pty`
pub(crate) async fn persistent_create(
    State(state): State<AppState>,
    body: Result<Json<PersistentCreate>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let Some(session_id) = payload.session_id.filter(|value| !value.is_empty()) else {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "sessionID is required to create a persistent PTY",
        )));
    };
    let cwd = payload
        .cwd
        .filter(|value| !value.is_empty())
        .map_or_else(|| state.directory().to_path_buf(), PathBuf::from);
    let value = create_persistent(
        &state,
        &session_id,
        payload.command,
        payload.args,
        cwd,
        payload.title,
        payload.env,
        payload.size,
    )?;
    Ok(Json(DataEnvelope::new(value)))
}

/// `GET /api/experimental/session/:sid/terminal`
#[allow(clippy::needless_collect)] // the collect releases the bindings lock
pub async fn terminal_list(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let manager = require_pty(&state)?;
    let ids: Vec<String> = lock(&BINDINGS)
        .iter()
        .filter(|(_, binding)| binding.session_id == sid)
        .map(|(id, _)| (*id).clone())
        .collect();
    let mut terminals = Vec::new();
    for id in ids {
        if let Some(info) = manager.info(&id) {
            terminals.push(persistent_info(&info, &sid));
        }
    }
    Ok(Json(DataEnvelope::new(json!(terminals))))
}

/// `POST /api/experimental/session/:sid/terminal`
pub(crate) async fn terminal_create(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
    body: Result<Json<PersistentCreate>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let cwd = match payload.cwd.filter(|value| !value.is_empty()) {
        Some(cwd) => PathBuf::from(cwd),
        None => session_directory(&state, &sid)
            .await
            .unwrap_or_else(|| state.directory().to_path_buf()),
    };
    let value = create_persistent(
        &state,
        &sid,
        payload.command,
        payload.args,
        cwd,
        payload.title,
        payload.env,
        payload.size,
    )?;
    Ok(Json(DataEnvelope::new(value)))
}

#[allow(clippy::too_many_arguments)]
fn create_persistent(
    state: &AppState,
    session_id: &str,
    command: Option<String>,
    args: Vec<String>,
    cwd: PathBuf,
    title: String,
    mut env: BTreeMap<String, String>,
    size: Option<PtySize>,
) -> Result<Value, ErrorResponse> {
    if !session_id.starts_with("ses") {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "sessionID must be a Session.ID",
        )));
    }
    let manager = require_pty(state)?;
    let (cols, rows) = size.map_or((80, 24), |size| (size.cols, size.rows));
    if cols == 0 || rows == 0 {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "pty size rows and cols must be positive",
        )));
    }
    env.insert("OPENCODE_SESSION_ID".to_owned(), session_id.to_owned());
    let info = manager
        .create(CreatePty {
            id: None,
            shell: command,
            args,
            cwd: cwd.clone(),
            location: Some(cwd),
            env,
            cols,
            rows,
            title: Some(title),
        })
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    lock(&BINDINGS).insert(
        info.id.clone(),
        Binding {
            session_id: session_id.to_owned(),
        },
    );
    lock(&CURRENT).insert(session_id.to_owned(), info.id.clone());
    let terminal = persistent_info(&info, session_id);
    let mut event = EventEnvelope::now(
        "persistent-pty.added",
        json!({ "sessionID": session_id, "terminal": terminal }),
    );
    event.location = Some(EventLocation {
        directory: state.directory().to_string_lossy().into_owned(),
    });
    state.publish(event);
    Ok(terminal)
}

/// `GET /api/experimental/persistent-pty/:id`
pub async fn persistent_get(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    Ok(Json(DataEnvelope::new(persistent_info(&info, &session_id))))
}

/// `PUT /api/experimental/persistent-pty/:id`
pub(crate) async fn persistent_update(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    body: Result<Json<PersistentUpdate>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(payload) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let Some(size) = payload.size else {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "InvalidRequestError",
            "size is required",
        )));
    };
    if size.cols == 0 || size.rows == 0 {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            "pty size rows and cols must be positive",
        )));
    }
    manager
        .resize(&id, size.cols, size.rows)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    lock(&CURRENT).insert(session_id.clone(), id.clone());
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    Ok(Json(DataEnvelope::new(persistent_info(&info, &session_id))))
}

/// `DELETE /api/experimental/persistent-pty/:id`
pub async fn persistent_remove(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, ErrorResponse> {
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    if info.running {
        let mut events = manager.subscribe();
        manager
            .terminate(&id)
            .map_err(|error| ErrorResponse::from(pty_error(error)))?;
        let _ = timeout(EXIT_WAIT, async {
            loop {
                match events.recv().await {
                    Ok(oc_pty::PtyEvent::Exited { id: event_id, .. }) if event_id == id => break,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
    }
    manager
        .remove(&id)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    lock(&BINDINGS).remove(&id);
    let mut current = lock(&CURRENT);
    if current.get(&session_id).is_some_and(|value| value == &id) {
        current.remove(&session_id);
    }
    drop(current);
    let mut event = EventEnvelope::now(
        "persistent-pty.removed",
        json!({ "sessionID": session_id, "ptyID": id }),
    );
    event.location = Some(EventLocation {
        directory: state.directory().to_string_lossy().into_owned(),
    });
    state.publish(event);
    Ok(no_content())
}

/// `POST /api/experimental/persistent-pty/:id/connect-token`
pub async fn persistent_connect_token(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    if headers
        .get(TICKET_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some("1")
    {
        return Err(ErrorResponse::from(ApiError::new(
            403,
            "ForbiddenError",
            "Invalid persistent PTY connect token request",
        )));
    }
    let manager = require_pty(&state)?;
    binding(&id)?;
    if manager.info(&id).is_none() {
        return Err(ErrorResponse::from(pty_not_found(&id)));
    }
    let resolved = location::resolve(&state, &headers, None, None);
    let (ticket, expires_in) = state.tickets().issue(&id, &resolved.directory);
    Ok(Json(DataEnvelope::new(
        json!({ "ticket": ticket, "expires_in": expires_in }),
    )))
}

/// `GET /api/experimental/persistent-pty/:id/snapshot`
pub async fn persistent_snapshot(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let manager = require_pty(&state)?;
    let session_id = binding(&id)?;
    let info = manager
        .info(&id)
        .ok_or_else(|| ErrorResponse::from(pty_not_found(&id)))?;
    let text = manager
        .snapshot(&id, usize::from(info.rows))
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    let replay = manager
        .replay(&id, None)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    let cursor = end_cursor(&text, info.cols, info.rows);
    Ok(Json(DataEnvelope::new(json!({
        "info": persistent_info(&info, &session_id),
        "text": text,
        "checkpoint": BASE64.encode(replay.bytes.as_ref()),
        "cursor": cursor,
    }))))
}

/// `GET /api/experimental/session/:sid/terminal/read`
pub(crate) async fn terminal_read(
    State(state): State<AppState>,
    AxumPath(sid): AxumPath<String>,
    params: Result<Query<ReadParams>, QueryRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let lines = match params.lines.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<usize>() {
            Ok(lines) if (1..=65_535).contains(&lines) => Some(lines),
            _ => {
                return Err(ErrorResponse::from(ApiError::service_unavailable(
                    "lines must be an integer between 1 and 65535",
                )));
            }
        },
    };
    let manager = require_pty(&state)?;
    let Some(id) = lock(&CURRENT).get(&sid).cloned() else {
        return Ok(Json(DataEnvelope::new(Value::Null)));
    };
    let Some(binding) = lock(&BINDINGS).get(&id).cloned() else {
        lock(&CURRENT).remove(&sid);
        return Ok(Json(DataEnvelope::new(Value::Null)));
    };
    if binding.session_id != sid {
        lock(&CURRENT).remove(&sid);
        return Ok(Json(DataEnvelope::new(Value::Null)));
    }
    let Some(info) = manager.info(&id) else {
        lock(&CURRENT).remove(&sid);
        return Ok(Json(DataEnvelope::new(Value::Null)));
    };
    let rows = lines.unwrap_or(usize::from(info.rows));
    let text = manager
        .snapshot(&id, rows)
        .map_err(|error| ErrorResponse::from(pty_error(error)))?;
    let cursor = end_cursor(&text, info.cols, info.rows);
    Ok(Json(DataEnvelope::new(json!({
        "ptyID": info.id,
        "title": info.title.clone().unwrap_or_default(),
        "cwd": info.cwd.to_string_lossy().into_owned(),
        "foregroundProcess": Value::Null,
        "screen": {
            "text": text,
            "cols": u32::from(info.cols.max(1)),
            "rows": u32::from(info.rows.max(1)),
            "cursor": cursor,
        },
    }))))
}

fn persistent_info(info: &PtyInfo, session_id: &str) -> Value {
    let mut value = Map::new();
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
    value.insert("sessionID".to_owned(), json!(session_id));
    value.insert("foregroundProcess".to_owned(), Value::Null);
    value.insert(
        "size".to_owned(),
        json!({ "cols": info.cols, "rows": info.rows }),
    );
    value.insert(
        "output".to_owned(),
        json!({ "head": info.output_head, "tail": info.output_bytes }),
    );
    Value::Object(value)
}

/// The wire cursor for the retained text: the position just past the last
/// printable character of the final row. `oc-pty` stores bytes, not cells, so
/// cursor-addressing sequences are not replayed here.
fn end_cursor(text: &str, cols: u16, rows: u16) -> Value {
    let cols = u32::from(cols.max(1));
    let rows = u32::from(rows.max(1));
    let lines: Vec<&str> = text.split('\n').collect();
    let y = u32::try_from(lines.len().saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(rows - 1);
    let width = lines.last().map_or(0, |line| {
        u32::try_from(line.chars().count()).unwrap_or(u32::MAX)
    });
    json!({ "x": width.min(cols - 1), "y": y })
}

fn binding(id: &str) -> Result<String, ErrorResponse> {
    lock(&BINDINGS)
        .get(id)
        .map(|binding| binding.session_id.clone())
        .ok_or_else(|| ErrorResponse::from(pty_not_found(id)))
}

async fn session_directory(state: &AppState, session_id: &str) -> Option<PathBuf> {
    let store = state.store()?;
    let session = store.get_session(session_id).await.ok()??;
    session
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn require_pty(state: &AppState) -> Result<&Arc<PtyManager>, ErrorResponse> {
    state.pty().ok_or_else(|| {
        ErrorResponse::from(ApiError::service_unavailable("pty manager not available"))
    })
}

fn pty_not_found(id: &str) -> ApiError {
    ApiError::not_found("PtyNotFoundError", format!("PTY session not found: {id}"))
        .with("ptyID", json!(id))
}

fn pty_error(error: PtyError) -> ApiError {
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
