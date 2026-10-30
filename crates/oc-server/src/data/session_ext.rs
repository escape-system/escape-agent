//! Remaining session surface: fork, revert, context, diff, environment, view,
//! forms, saved permissions, export/import, statistics, skill activation and
//! instruction entries.
//!
//! Every handler is a thin projection over the store. Revert consumes the
//! file snapshots the runtime captures before mutating tools run: staging
//! diffs the earliest snapshot of every path against the current file, clearing
//! restores those snapshots, and committing truncates the tail. Form lifecycle
//! changes are published as ephemeral events, while view stamps and forks
//! append durable events so the session log stays authoritative.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{delete, get, patch as patch_route, post, put};
use oc_store::{
    Delivery, MessageQuery, NewForm, NewInbox, NewMessage, NewSession, Order, SessionPatch,
    SessionQuery, SnapshotRow, Store, StoreError,
};
use oc_tools::diff::file_diff;
use oc_tools::util::atomic_write;
use oc_types::{ApiError, EventEnvelope, EventLocation, MessageId, SessionId, now_millis};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::fs::confined_target;
use crate::data::location;
use crate::data::session::{notify, publish_enqueued};
use crate::data::{
    no_content, query_error, require_config, require_store, schema_error, session_not_found,
};
use crate::error::ErrorResponse;

/// Page size used by every full-scan helper (the store clamps to 200).
const PAGE: usize = 200;

/// Largest accepted instruction entry, matching the documented 1 MiB cap.
const INSTRUCTION_MAX_BYTES: usize = 1024 * 1024;

/// First value beyond `i64::MAX` that is representable as `f64` (2^63).
const I64_F64_LIMIT: f64 = 9_223_372_036_854_775_808.0;

/// Routes owned by this module.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/session/{sid}/fork", post(fork))
        .route("/api/session/{sid}/revert/stage", post(revert_stage))
        .route("/api/session/{sid}/revert", delete(revert_clear))
        .route("/api/session/{sid}/revert/commit", post(revert_commit))
        .route("/api/session/{sid}/context", get(context))
        .route("/api/session/{sid}/diff", get(diff))
        .route("/api/session/{sid}/environment", put(environment))
        .route("/api/session/{sid}/view", post(view))
        .route("/api/session/{sid}/form", get(form_list).post(form_create))
        .route(
            "/api/session/{sid}/form/{form_id}",
            get(form_get).delete(form_cancel),
        )
        .route("/api/session/{sid}/form/{form_id}/reply", post(form_reply))
        .route("/api/permission/saved", get(saved_permissions))
        .route(
            "/api/permission/saved/{id}",
            patch_route(saved_permission_update).delete(saved_permission_delete),
        )
        .route("/api/experimental/session/{sid}/export", get(export))
        .route("/api/experimental/session/import", post(import))
        .route("/api/experimental/session/stats", get(stats))
        .route("/api/experimental/session/{sid}/skill", post(skill))
        .route(
            "/api/experimental/session/{sid}/instructions/entries",
            get(instruction_list),
        )
        .route(
            "/api/experimental/session/{sid}/instructions/entries/{key}",
            put(instruction_put).delete(instruction_delete),
        )
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Loads a session or answers `SessionNotFoundError`.
async fn require_session(store: &Store, session_id: &str) -> Result<Value, ErrorResponse> {
    store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id).into())
}

/// Reads every message of a session in ascending `seq` order.
async fn messages_all(store: &Store, session_id: &str) -> Result<Vec<Value>, StoreError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store
            .list_messages(MessageQuery {
                session_id: session_id.to_owned(),
                limit: PAGE,
                order: Order::Asc,
                cursor,
                kind: None,
            })
            .await?;
        items.extend(page.items);
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(items)
}

/// Reads every session, optionally filtered by project.
async fn sessions_all(store: &Store, project: Option<&str>) -> Result<Vec<Value>, StoreError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = store
            .list_sessions(SessionQuery {
                limit: PAGE,
                project: project.map(str::to_owned),
                cursor,
                ..SessionQuery::default()
            })
            .await?;
        items.extend(page.items);
        match page.cursor.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(items)
}

fn message_not_found(session_id: &str, message_id: &str) -> ApiError {
    ApiError::not_found(
        "MessageNotFoundError",
        format!("Message not found: {message_id}"),
    )
    .with("sessionID", json!(session_id))
    .with("messageID", json!(message_id))
}

fn form_not_found(id: &str) -> ApiError {
    ApiError::not_found("FormNotFoundError", format!("Form not found: {id}")).with("id", json!(id))
}

fn form_settled(id: &str) -> ApiError {
    ApiError::new(
        409,
        "FormAlreadySettledError",
        format!("Form {id} has already been answered or cancelled"),
    )
    .with("id", json!(id))
}

fn form_invalid(id: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(400, "FormInvalidAnswerError", message).with("id", json!(id))
}

fn map_form_store_error(error: StoreError, id: &str) -> ErrorResponse {
    match error {
        StoreError::NotFound(_) => form_not_found(id).into(),
        StoreError::Conflict(_) => form_settled(id).into(),
        other => other.into(),
    }
}

/// Truncates a finite float to `i64` without a lossy `as` cast, saturating at
/// the integer bounds exactly like a numeric cast would.
fn float_i64(number: f64) -> Option<i64> {
    if !number.is_finite() {
        return None;
    }
    let truncated = number.trunc();
    if truncated >= I64_F64_LIMIT {
        return Some(i64::MAX);
    }
    if truncated <= -I64_F64_LIMIT {
        return Some(i64::MIN);
    }
    format!("{truncated:.0}").parse::<i64>().ok()
}

/// Reads a JSON number as `i64`, keeping the old truncation behavior for
/// values stored as floats.
fn json_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_u64()
            .and_then(|number| i64::try_from(number).ok())
            .or_else(|| value.as_f64().and_then(float_i64))
    })
}

fn time_of(value: &Value) -> Option<i64> {
    value.pointer("/time/created").and_then(json_i64)
}

fn id_of(value: &Value) -> String {
    value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The stored message body: identity fields belong to the row, not the body.
fn message_body(message: &Value) -> Value {
    let mut map = message.as_object().cloned().unwrap_or_default();
    map.remove("id");
    map.remove("sessionID");
    map.remove("type");
    Value::Object(map)
}

fn non_null(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| !value.is_null()).cloned()
}

/// Publishes an ephemeral event tagged with the session's location.
///
/// Ephemeral events carry no durable marker: they are live notifications, not
/// entries in the session log.
fn publish_ephemeral(state: &AppState, session: &Value, kind: &str, data: Value) {
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map_or_else(
            || state.directory().to_string_lossy().into_owned(),
            str::to_owned,
        );
    let mut event = EventEnvelope::now(kind, data);
    event.location = Some(EventLocation { directory });
    state.publish(event);
}

// ---------------------------------------------------------------------------
// Fork
// ---------------------------------------------------------------------------

/// `POST /api/session/:sid/fork`
#[derive(Debug, Default, Deserialize)]
pub struct ForkBody {
    #[serde(default)]
    before: Option<String>,
}

/// `POST /api/session/:sid/fork`
#[allow(clippy::too_many_lines)] // one linear copy-then-publish handler; splitting hides commit order
pub async fn fork(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<ForkBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let parent = require_session(store, &session_id).await?;
    let messages = messages_all(store, &session_id).await?;

    // `before` excludes the boundary message; without it the whole history is
    // copied (upstream's `through` boundary).
    let requested = body.before.as_deref().filter(|id| !id.is_empty());
    let cut = match requested {
        Some(before) => messages
            .iter()
            .position(|message| id_of(message) == before)
            .ok_or_else(|| message_not_found(&session_id, before))?,
        None => messages.len(),
    };
    // A fork with no history has no boundary to report; upstream refuses that
    // fork entirely, so no event is published rather than inventing a message.
    let boundary = match requested {
        Some(before) => Some(json!({ "type": "before", "messageID": before })),
        None => messages
            .last()
            .map(|message| json!({ "type": "through", "messageID": id_of(message) })),
    };

    let id = SessionId::new().into_inner();
    let directory = parent
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let project_id = parent
        .get("projectID")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let session = store
        .create_session(NewSession {
            id,
            parent_id: None,
            project_id,
            directory,
            title: parent
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            agent: parent
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_owned),
            model: non_null(parent.get("model")),
            metadata: non_null(parent.get("metadata")),
            permissions: None,
            time_created: now_millis(),
        })
        .await?;

    // Identity is assigned up front so compaction `keepID` references can be
    // remapped before any row is written.
    let new_session_id = id_of(&session);
    let mut identities: HashMap<String, String> = HashMap::new();
    for message in &messages[..cut] {
        identities.insert(id_of(message), MessageId::new().into_inner());
    }

    for message in &messages[..cut] {
        let old_id = id_of(message);
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if kind.is_empty() {
            continue;
        }
        let mut data = message_body(message);
        let kept_id = if kind == "compaction" {
            data.pointer("/metadata/keepID")
                .and_then(Value::as_str)
                .and_then(|kept| identities.get(kept))
                .cloned()
        } else {
            None
        };
        if let Some(kept) = kept_id
            && let Some(metadata) = data.get_mut("metadata").and_then(Value::as_object_mut)
        {
            metadata.insert("keepID".to_owned(), Value::String(kept));
        }
        let new_id = identities
            .get(&old_id)
            .cloned()
            .unwrap_or_else(|| MessageId::new().into_inner());
        store
            .append_message(NewMessage {
                id: new_id,
                session_id: new_session_id.clone(),
                kind,
                data,
                time_created: time_of(message).unwrap_or_else(now_millis),
            })
            .await?;
    }

    // The fork (session row plus copied history) is committed; the durable
    // event records the parent boundary for every follower of the child log.
    if let Some(boundary) = boundary {
        publish_durable(
            &state,
            &new_session_id,
            "session.forked",
            json!({
                "sessionID": new_session_id,
                "parentID": session_id,
                "boundary": boundary,
            }),
            2,
        )
        .await?;
    }

    Ok(Json(json!({ "data": session })))
}

// ---------------------------------------------------------------------------
// Revert
// ---------------------------------------------------------------------------

/// Canonical session directory; every snapshot path is confined to it.
fn session_root(state: &AppState, session: &Value) -> PathBuf {
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map_or_else(|| state.directory().to_path_buf(), PathBuf::from);
    std::fs::canonicalize(&directory).unwrap_or(directory)
}

/// Dense sequence of a message, if it still exists.
async fn boundary_seq(
    store: &Store,
    session_id: &str,
    message_id: &str,
) -> Result<Option<i64>, StoreError> {
    Ok(store
        .message_seq(session_id, message_id)
        .await?
        .and_then(|seq| i64::try_from(seq).ok()))
}

/// The diff of one snapshot against the current file.
///
/// Only regular files inside the session directory take part. A skipped
/// snapshot has no comparable pre-image, and a path that leaves the directory
/// belongs to someone else, so both are ignored.
fn snapshot_diff(root: &std::path::Path, row: &SnapshotRow) -> Option<Value> {
    if row.skipped {
        return None;
    }
    let target = confined_target(root, &row.path).ok()?;
    let current = match std::fs::metadata(&target) {
        Ok(meta) if meta.is_file() => Some(std::fs::read(&target).ok()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Ok(_) | Err(_) => return None,
    };
    let before = if row.existed {
        let content = row.content.as_deref()?;
        Some(String::from_utf8_lossy(content).into_owned())
    } else {
        None
    };
    let after = current.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let (status, before, after) = match (before, after) {
        (None, None) => return None,
        (None, Some(after)) => ("added", String::new(), after),
        (Some(before), None) => ("deleted", before, String::new()),
        (Some(before), Some(after)) => {
            if before == after {
                return None;
            }
            ("modified", before, after)
        }
    };
    Some(file_diff(&row.path, &before, &after, status))
}

/// Recomputes the staged revert diffs from the snapshots and the current tree.
async fn revert_diffs(
    store: &Store,
    state: &AppState,
    session: &Value,
    session_id: &str,
    boundary: i64,
) -> Result<Vec<Value>, StoreError> {
    let root = session_root(state, session);
    let rows = store.earliest_snapshots_after(session_id, boundary).await?;
    Ok(rows
        .iter()
        .filter_map(|row| snapshot_diff(&root, row))
        .collect())
}

/// Restores one earliest snapshot, leaving anything it cannot safely own.
fn restore_snapshot(root: &std::path::Path, row: &SnapshotRow) {
    if row.skipped {
        tracing::warn!(path = %row.path, "snapshot was skipped; leaving the path untouched");
        return;
    }
    let Ok(target) = confined_target(root, &row.path) else {
        tracing::warn!(
            path = %row.path,
            "snapshot path escapes the session directory; leaving it untouched"
        );
        return;
    };
    let existing = std::fs::metadata(&target);
    if row.existed {
        let Some(content) = row.content.as_deref() else {
            tracing::warn!(path = %row.path, "snapshot has no content; leaving the path untouched");
            return;
        };
        match &existing {
            Ok(meta) if !meta.is_file() => {
                tracing::warn!(
                    path = %row.path,
                    "snapshot target is not a regular file; leaving it untouched"
                );
                return;
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %row.path,
                    "snapshot target is unreadable; leaving it untouched"
                );
                return;
            }
            _ => {}
        }
        if let Err(error) = atomic_write(&target, content) {
            tracing::warn!(error = %error, path = %row.path, "snapshot restore failed");
        }
    } else {
        match existing {
            Ok(meta) if meta.is_file() => {
                if let Err(error) = std::fs::remove_file(&target) {
                    tracing::warn!(error = %error, path = %row.path, "snapshot removal failed");
                }
            }
            Ok(_) => tracing::warn!(
                path = %row.path,
                "snapshot target is not a regular file; leaving it untouched"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => tracing::warn!(
                path = %row.path,
                "snapshot target is unreadable; leaving it untouched"
            ),
        }
    }
}

/// Restores the earliest snapshot of every path captured at or after `boundary`.
async fn restore_snapshots(
    store: &Store,
    state: &AppState,
    session: &Value,
    session_id: &str,
    boundary: i64,
) -> Result<(), StoreError> {
    let root = session_root(state, session);
    for row in store.earliest_snapshots_after(session_id, boundary).await? {
        restore_snapshot(&root, &row);
    }
    Ok(())
}

/// `POST /api/session/:sid/revert/stage`
#[derive(Debug, Deserialize)]
pub struct RevertStageBody {
    #[serde(rename = "messageID")]
    message_id: String,
    #[serde(default)]
    #[allow(dead_code)] // accepted for wire compatibility; the diff is always computed
    files: Option<bool>,
}

/// `POST /api/session/:sid/revert/stage`
pub async fn revert_stage(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<RevertStageBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let boundary = boundary_seq(store, &session_id, &body.message_id)
        .await?
        .ok_or_else(|| message_not_found(&session_id, &body.message_id))?;

    let files = revert_diffs(store, &state, &session, &session_id, boundary).await?;
    let revert = json!({ "messageID": body.message_id, "files": files });
    store
        .patch_session(
            &session_id,
            SessionPatch {
                revert: Some(revert.clone()),
                ..SessionPatch::default()
            },
        )
        .await?;
    Ok(Json(json!({ "data": revert })))
}

/// `DELETE /api/session/:sid/revert`
pub async fn revert_clear(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let boundary = session
        .pointer("/revert/messageID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(message_id) = boundary {
        if let Some(seq) = boundary_seq(store, &session_id, &message_id).await? {
            restore_snapshots(store, &state, &session, &session_id, seq).await?;
            store.delete_snapshots_after(&session_id, seq).await?;
        } else {
            tracing::warn!(
                session_id = %session_id,
                message_id = %message_id,
                "revert boundary message is missing; snapshots were not restored"
            );
        }
        store
            .patch_session(
                &session_id,
                SessionPatch {
                    revert: Some(Value::Null),
                    ..SessionPatch::default()
                },
            )
            .await?;
    }
    Ok(no_content())
}

/// `POST /api/session/:sid/revert/commit`
pub async fn revert_commit(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let boundary = session
        .pointer("/revert/messageID")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(message_id) = boundary {
        // Commit deletes the boundary message and everything after it, matching
        // the documented revert boundary, and drops the snapshots that fed it.
        if let Some(seq) = boundary_seq(store, &session_id, &message_id).await? {
            store
                .truncate_session(&session_id, u64::try_from(seq).unwrap_or_default())
                .await?;
            store.delete_snapshots_after(&session_id, seq).await?;
        }
        store
            .patch_session(
                &session_id,
                SessionPatch {
                    revert: Some(Value::Null),
                    ..SessionPatch::default()
                },
            )
            .await?;
    }
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Context and diff
// ---------------------------------------------------------------------------

/// `GET /api/session/:sid/context`
pub async fn context(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let messages = messages_all(store, &session_id).await?;

    // The model view starts after the newest completed compaction.
    let start = messages
        .iter()
        .rposition(|message| {
            message.get("type").and_then(Value::as_str) == Some("compaction")
                && message
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| status == "completed")
        })
        .map_or(0, |index| index + 1);

    Ok(Json(json!({ "data": &messages[start..] })))
}

/// `GET /api/session/:sid/diff`
#[derive(Debug, Default, Deserialize)]
pub struct DiffParams {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // accepted; diffs are recomputed with a single full-file hunk
    context: Option<String>,
}

/// `GET /api/session/:sid/diff`
pub async fn diff(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<DiffParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    for message_id in [params.from.as_deref(), params.to.as_deref()]
        .into_iter()
        .flatten()
    {
        if store.get_message(&session_id, message_id).await?.is_none() {
            return Err(message_not_found(&session_id, message_id).into());
        }
    }

    // A staged revert recomputes against the tree, so a later edit is visible
    // instead of being masked by the cached list stored on staging.
    let mut files = Vec::new();
    if let Some(message_id) = session.pointer("/revert/messageID").and_then(Value::as_str)
        && let Some(seq) = boundary_seq(store, &session_id, message_id).await?
    {
        files = revert_diffs(store, &state, &session, &session_id, seq).await?;
    }
    Ok(Json(json!({ "data": files })))
}

// ---------------------------------------------------------------------------
// Environment and view
// ---------------------------------------------------------------------------

/// `PUT /api/session/:sid/environment`
#[derive(Debug, Deserialize)]
pub struct EnvironmentBody {
    variables: Map<String, Value>,
}

/// `PUT /api/session/:sid/environment`
pub async fn environment(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<EnvironmentBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.variables.values().any(|value| !value.is_string()) {
        return Err(ApiError::new(400, "BadRequestError", "variables must map to strings").into());
    }
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let mut metadata = session
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert("environment".to_owned(), Value::Object(body.variables));
    store
        .patch_session(
            &session_id,
            SessionPatch {
                metadata: Some(Value::Object(metadata)),
                ..SessionPatch::default()
            },
        )
        .await?;
    Ok(no_content())
}

/// `POST /api/session/:sid/view`
#[derive(Debug, Deserialize)]
pub struct ViewBody {
    idle: f64,
}

/// `POST /api/session/:sid/view`
pub async fn view(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<ViewBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if !body.idle.is_finite() || body.idle < 0.0 {
        return Err(ApiError::new(400, "BadRequestError", "idle must be an epoch in ms").into());
    }
    let Some(idle) = float_i64(body.idle) else {
        return Err(ApiError::new(400, "BadRequestError", "idle must be an epoch in ms").into());
    };
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    store.touch_session_viewed(&session_id, idle).await?;
    publish_durable(
        &state,
        &session_id,
        "session.viewed",
        json!({ "sessionID": session_id, "idle": idle }),
        1,
    )
    .await?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

/// `POST /api/session/:sid/form`
#[derive(Debug, Deserialize)]
pub struct FormCreateBody {
    #[serde(default)]
    id: Option<String>,
    title: String,
    #[serde(default)]
    metadata: Option<Value>,
    fields: Value,
}

/// `POST /api/session/:sid/form/:form_id/reply`
#[derive(Debug, Deserialize)]
pub struct FormReplyBody {
    answer: Value,
}

/// `DELETE /api/session/:sid/form/:form_id?message=`
#[derive(Debug, Default, Deserialize)]
pub struct FormCancelParams {
    #[serde(default)]
    message: Option<String>,
}

const FIELD_KINDS: [&str; 6] = [
    "string",
    "number",
    "integer",
    "boolean",
    "multiselect",
    "external",
];

fn invalid_form(message: impl Into<String>) -> ApiError {
    ApiError::new(400, "BadRequestError", message)
}

fn validate_form_fields(fields: &Value) -> Result<(), ApiError> {
    let items = fields
        .as_array()
        .ok_or_else(|| invalid_form("fields must be an array"))?;
    if items.is_empty() {
        return Err(invalid_form("fields must not be empty"));
    }
    let mut keys = HashSet::new();
    for field in items {
        let object = field
            .as_object()
            .ok_or_else(|| invalid_form("each field must be an object"))?;
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| invalid_form("each field needs a non-empty key"))?;
        if !keys.insert(key.to_owned()) {
            return Err(invalid_form(format!("duplicate field key: {key}")));
        }
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_form(format!("field {key} needs a type")))?;
        if !FIELD_KINDS.contains(&kind) {
            return Err(invalid_form(format!(
                "field {key} has unknown type {kind:?}"
            )));
        }
        if kind == "external" && object.get("url").and_then(Value::as_str).is_none() {
            return Err(invalid_form(format!("external field {key} needs a url")));
        }
    }
    Ok(())
}

fn validate_answer(detail: &Value, answer: &Value) -> Result<(), String> {
    let answer = answer
        .as_object()
        .ok_or_else(|| "answer must be an object".to_owned())?;
    let fields = detail
        .get("fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut known = HashSet::new();
    for field in &fields {
        let Some(object) = field.as_object() else {
            continue;
        };
        let Some(key) = object.get("key").and_then(Value::as_str) else {
            continue;
        };
        known.insert(key.to_owned());
        let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
        let required = object
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hidden = object
            .get("hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(value) = answer.get(key) else {
            if required && !hidden {
                return Err(format!("missing required field {key}"));
            }
            continue;
        };
        let valid = match kind {
            "string" => value.is_string(),
            "number" => value.is_number() || is_special_number(value),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "multiselect" => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            _ => true,
        };
        if !valid {
            return Err(format!("field {key} does not accept this value"));
        }
    }
    for key in answer.keys() {
        if !known.contains(key) {
            return Err(format!("unknown field {key}"));
        }
    }
    Ok(())
}

fn is_special_number(value: &Value) -> bool {
    matches!(value.as_str(), Some("Infinity" | "-Infinity" | "NaN"))
}

/// `GET /api/session/:sid/form`
pub async fn form_list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let forms = store.list_forms(Some(session_id.as_str())).await?;
    Ok(Json(json!({ "data": forms })))
}

/// `POST /api/session/:sid/form`
pub async fn form_create(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<FormCreateBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.title.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "title must not be empty").into());
    }
    if let Some(metadata) = &body.metadata
        && !metadata.is_object()
    {
        return Err(ApiError::new(400, "BadRequestError", "metadata must be an object").into());
    }
    validate_form_fields(&body.fields)?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let form = store
        .create_form(NewForm {
            id: body.id.filter(|id| !id.is_empty()),
            session_id: session_id.clone(),
            title: body.title,
            metadata: body.metadata,
            fields: body.fields,
        })
        .await?;
    publish_ephemeral(&state, &session, "form.created", json!({ "form": &form }));
    Ok(Json(json!({ "data": form })))
}

/// `GET /api/session/:sid/form/:form_id`
pub async fn form_get(
    Path((session_id, form_id)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let form = store
        .get_form(&session_id, &form_id)
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    Ok(Json(json!({ "data": form })))
}

/// `POST /api/session/:sid/form/:form_id/reply`
pub async fn form_reply(
    Path((session_id, form_id)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Result<Json<FormReplyBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    let detail = store
        .get_form(&session_id, &form_id)
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    let status = detail
        .pointer("/state/status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if status != "pending" {
        return Err(form_settled(&form_id).into());
    }
    validate_answer(&detail, &body.answer).map_err(|message| form_invalid(&form_id, message))?;
    let answer = body.answer;
    store
        .reply_form(&session_id, &form_id, &json!({ "answer": &answer }))
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    publish_ephemeral(
        &state,
        &session,
        "form.replied",
        json!({ "id": form_id, "sessionID": session_id, "answer": answer }),
    );
    Ok(no_content())
}

/// `DELETE /api/session/:sid/form/:form_id`
pub async fn form_cancel(
    Path((session_id, form_id)): Path<(String, String)>,
    State(state): State<AppState>,
    params: Result<Query<FormCancelParams>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let session = require_session(store, &session_id).await?;
    store
        .cancel_form_with_message(&session_id, &form_id, params.message.as_deref())
        .await
        .map_err(|error| map_form_store_error(error, &form_id))?;
    publish_ephemeral(
        &state,
        &session,
        "form.cancelled",
        json!({ "id": form_id, "sessionID": session_id }),
    );
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Saved permissions
// ---------------------------------------------------------------------------

/// `GET /api/permission/saved?projectID=`
#[derive(Debug, Default, Deserialize)]
pub struct SavedPermissionParams {
    #[serde(default, rename = "projectID")]
    project_id: Option<String>,
}

/// `PATCH /api/permission/saved/:id`
#[derive(Debug, Default, Deserialize)]
pub struct SavedPermissionPatch {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

fn permission_not_found(id: &str) -> ApiError {
    ApiError::not_found(
        "PermissionSavedNotFoundError",
        format!("Saved permission not found: {id}"),
    )
    .with("id", json!(id))
}

/// `GET /api/permission/saved`
pub async fn saved_permissions(
    State(state): State<AppState>,
    params: Result<Query<SavedPermissionParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let store = require_store(&state)?;
    let rows = store
        .list_saved_permissions(params.project_id.as_deref())
        .await?;
    Ok(Json(json!({ "data": rows })))
}

/// `PATCH /api/permission/saved/:id`
pub async fn saved_permission_update(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<SavedPermissionPatch>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let updated = store
        .update_saved_permission(&id, None, body.action.as_deref(), body.resource.as_deref())
        .await?;
    let updated = updated.ok_or_else(|| permission_not_found(&id))?;
    Ok(Json(json!({ "data": updated })))
}

/// `DELETE /api/permission/saved/:id`
pub async fn saved_permission_delete(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store.delete_saved_permission(&id).await?;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Export and import
// ---------------------------------------------------------------------------

/// `GET /api/experimental/session/:sid/export?sanitize=`
#[derive(Debug, Default, Deserialize)]
pub struct ExportParams {
    #[serde(default)]
    sanitize: Option<String>,
}

/// `POST /api/experimental/session/import`
#[derive(Debug, Deserialize)]
pub struct ImportBody {
    info: Value,
    messages: Vec<Value>,
    #[serde(default)]
    location: Option<ImportLocation>,
}

#[derive(Debug, Deserialize)]
pub struct ImportLocation {
    directory: String,
}

const SECRET_KEYS: [&str; 13] = [
    "authorization",
    "apikey",
    "api_key",
    "api-key",
    "token",
    "access_token",
    "refresh_token",
    "secret",
    "client_secret",
    "password",
    "credential",
    "credentials",
    "private_key",
];

fn is_secret_key(key: &str) -> bool {
    SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str())
}

/// Drops credential-bearing keys recursively. Export-only; never mutates the
/// stored transcript.
fn sanitize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|key, _| !is_secret_key(key));
            for item in map.values_mut() {
                sanitize(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                sanitize(item);
            }
        }
        _ => {}
    }
}

/// `GET /api/experimental/session/:sid/export`
pub async fn export(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<ExportParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let sanitize_requested = match params.sanitize.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(other) => {
            return Err(ApiError::new(
                400,
                "InvalidRequestError",
                format!("sanitize must be true or false, got {other:?}"),
            )
            .into());
        }
    };
    let store = require_store(&state)?;
    let info = require_session(store, &session_id).await?;
    let mut messages = messages_all(store, &session_id).await?;
    let data = if sanitize_requested {
        let mut info = info;
        sanitize(&mut info);
        for message in &mut messages {
            sanitize(message);
        }
        json!({ "info": info, "messages": messages })
    } else {
        json!({ "info": info, "messages": messages })
    };
    Ok(Json(json!({ "data": data })))
}

/// `POST /api/experimental/session/import`
#[allow(clippy::too_many_lines)] // one linear create-then-copy handler; splitting hides rollback
pub async fn import(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ImportBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let info = body
        .info
        .as_object()
        .cloned()
        .ok_or_else(|| ApiError::new(400, "InvalidRequestError", "info must be an object"))?;
    let session_id = info
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::new(400, "InvalidRequestError", "info.id is required"))?
        .to_owned();
    let parent_id = info
        .get("parentID")
        .and_then(Value::as_str)
        .filter(|parent| !parent.is_empty())
        .map(str::to_owned);

    let store = require_store(&state)?;
    if store.get_session(&session_id).await?.is_some() {
        return Err(ApiError::conflict(format!("Session {session_id} already exists")).into());
    }
    if let Some(parent) = &parent_id
        && store.get_session(parent).await?.is_none()
    {
        return Err(session_not_found(parent).into());
    }
    for message in &body.messages {
        let kind = message.get("type").and_then(Value::as_str);
        if kind.is_none_or(str::is_empty) {
            return Err(
                ApiError::new(400, "InvalidRequestError", "each message needs a type").into(),
            );
        }
    }

    let requested = body
        .location
        .as_ref()
        .map(|location| location.directory.as_str())
        .or_else(|| {
            info.get("location")
                .and_then(|location| location.get("directory"))
                .and_then(Value::as_str)
        });
    let resolved = location::resolve(&state, &headers, requested, None);
    if !resolved.path().is_dir() {
        return Err(ApiError::not_found(
            "LocationNotFoundError",
            format!("Directory not found: {}", resolved.directory),
        )
        .with("location", json!({ "directory": resolved.directory }))
        .into());
    }

    let time_created = info
        .get("time")
        .and_then(|time| time.get("created"))
        .and_then(Value::as_i64)
        .unwrap_or_else(now_millis);
    let session = store
        .create_session(NewSession {
            id: session_id.clone(),
            parent_id,
            project_id: resolved.project.id.as_str().to_owned(),
            directory: resolved.directory.clone(),
            title: info.get("title").and_then(Value::as_str).map(str::to_owned),
            agent: info.get("agent").and_then(Value::as_str).map(str::to_owned),
            model: non_null(info.get("model")),
            metadata: non_null(info.get("metadata")),
            permissions: non_null(info.get("permissions")),
            time_created,
        })
        .await?;

    // `session_message.id` is a database-wide primary key, while
    // `append_message` only checks duplicates inside the target session, so a
    // transcript imported next to its source would collide. Assign every copied
    // message a fresh identity and remap compaction `keepID` references, as fork
    // does.
    let mut identities: HashMap<String, String> = HashMap::new();
    for message in &body.messages {
        if let Some(old) = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            identities.insert(old.to_owned(), MessageId::new().into_inner());
        }
    }

    for message in body.messages {
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let old_id = message
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let id = old_id
            .as_ref()
            .and_then(|old| identities.get(old))
            .cloned()
            .unwrap_or_else(|| MessageId::new().into_inner());
        let mut data = message_body(&message);
        let kept_id = if kind == "compaction" {
            data.pointer("/metadata/keepID")
                .and_then(Value::as_str)
                .and_then(|kept| identities.get(kept))
                .cloned()
        } else {
            None
        };
        if let Some(kept) = kept_id
            && let Some(metadata) = data.get_mut("metadata").and_then(Value::as_object_mut)
        {
            metadata.insert("keepID".to_owned(), Value::String(kept));
        }
        let appended = store
            .append_message(NewMessage {
                id,
                session_id: session_id.clone(),
                kind,
                data,
                time_created: time_of(&message).unwrap_or(time_created),
            })
            .await;
        if let Err(error) = appended {
            // A half-imported session would be worse than no import at all.
            let _ = store.delete_session(&session_id).await;
            return Err(error.into());
        }
    }

    Ok(Json(json!({ "data": session })))
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// `GET /api/experimental/session/stats`
#[derive(Debug, Default, Deserialize)]
pub struct StatsParams {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    tools: Option<String>,
}

struct ScannedMessage {
    session_id: String,
    parent_id: Option<String>,
    kind: String,
    time: i64,
    data: Value,
}

fn parse_epoch(raw: &str, field: &str) -> Result<i64, ApiError> {
    if let Ok(value) = raw.parse::<i64>() {
        return Ok(value);
    }
    if let Ok(value) = raw.parse::<f64>()
        && let Some(value) = float_i64(value)
    {
        return Ok(value);
    }
    Err(ApiError::new(
        400,
        "InvalidRequestError",
        format!("{field} must be an epoch in milliseconds"),
    ))
}

fn number_at(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(json_i64).unwrap_or(0)
}

fn float_at(value: &Value, pointer: &str) -> f64 {
    value
        .pointer(pointer)
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn empty_tokens() -> Value {
    json!({ "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } })
}

fn add_tokens(total: &mut Value, message: &Value) {
    for (target, source) in [
        ("/input", "/tokens/input"),
        ("/output", "/tokens/output"),
        ("/reasoning", "/tokens/reasoning"),
        ("/cache/read", "/tokens/cache/read"),
        ("/cache/write", "/tokens/cache/write"),
    ] {
        let current = number_at(total, target);
        let add = number_at(message, source);
        if let Some(slot) = total.pointer_mut(target) {
            *slot = json!(current + add);
        }
    }
}

fn tokens_total(tokens: &Value) -> i64 {
    number_at(tokens, "/input")
        + number_at(tokens, "/output")
        + number_at(tokens, "/reasoning")
        + number_at(tokens, "/cache/read")
        + number_at(tokens, "/cache/write")
}

/// Days since the Unix epoch to a civil `(year, month, day)` (Hinnant).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn day_ordinal(millis: i64) -> i64 {
    millis.div_euclid(86_400_000)
}

fn date_key(millis: i64) -> String {
    let (year, month, day) = civil_from_days(day_ordinal(millis));
    format!("{year:04}-{month:02}-{day:02}")
}

fn median(values: &mut [u32]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::from(values[middle - 1]).midpoint(f64::from(values[middle]))
    } else {
        f64::from(values[middle])
    }
}

#[derive(Default)]
struct ToolAggregate {
    calls: i64,
    succeeded: i64,
    failed: i64,
    unfinished: i64,
    durations: Vec<u32>,
}

/// `GET /api/experimental/session/stats`
#[allow(clippy::too_many_lines)] // one full-scan aggregator; splitting hides the shared accumulators
pub async fn stats(
    State(state): State<AppState>,
    params: Result<Query<StatsParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    if let Some(timezone) = params.timezone.as_deref()
        && !matches!(timezone, "UTC" | "Etc/UTC" | "utc")
    {
        // Only UTC is supported offline; refuse rather than bucket by the
        // wrong civil day.
        return Err(ApiError::new(
            400,
            "InvalidRequestError",
            format!("Unsupported timezone {timezone:?}; only UTC is available"),
        )
        .into());
    }
    let tools_mode = params.tools.as_deref().unwrap_or("summary");
    if !matches!(tools_mode, "none" | "summary" | "detail") {
        return Err(ApiError::new(
            400,
            "InvalidRequestError",
            format!("tools must be none, summary or detail, got {tools_mode:?}"),
        )
        .into());
    }

    let store = require_store(&state)?;
    let to = match params.to.as_deref() {
        Some(raw) => parse_epoch(raw, "to")?,
        None => now_millis(),
    };

    let sessions = sessions_all(store, params.project.as_deref()).await?;
    let mut scanned: Vec<ScannedMessage> = Vec::new();
    let mut earliest: Option<i64> = None;
    for session in &sessions {
        let session_id = id_of(session);
        let parent_id = session
            .get("parentID")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let created = session
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        for message in messages_all(store, &session_id).await? {
            let kind = message
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if kind != "user" && kind != "assistant" {
                continue;
            }
            let time = time_of(&message).unwrap_or_default();
            // Forked sessions carry a copy of the parent's history; only rows
            // recorded after the fork count.
            if time < created || time >= to {
                continue;
            }
            earliest = Some(earliest.map_or(time, |current| current.min(time)));
            scanned.push(ScannedMessage {
                session_id: session_id.clone(),
                parent_id: parent_id.clone(),
                kind,
                time,
                data: message,
            });
        }
    }

    let explicit_from = params.from.is_some();
    let from = match params.from.as_deref() {
        Some(raw) => parse_epoch(raw, "from")?,
        None => earliest.unwrap_or(to),
    };
    if explicit_from && from >= to {
        return Err(
            ApiError::new(400, "InvalidRequestError", "from must be earlier than to").into(),
        );
    }

    let mut unique_sessions = HashSet::new();
    let mut subagents = HashSet::new();
    let mut prompts = 0_i64;
    let mut steps = 0_i64;
    let mut tokens = empty_tokens();
    let mut cost = 0.0_f64;
    let mut activity: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    let mut models: HashMap<String, (Value, i64, Value, f64)> = HashMap::new();
    let mut tools: HashMap<String, ToolAggregate> = HashMap::new();

    for message in &scanned {
        if message.time < from {
            continue;
        }
        if message.parent_id.is_none() {
            unique_sessions.insert(message.session_id.clone());
        } else {
            subagents.insert(message.session_id.clone());
        }
        if message.kind == "user" {
            if message.parent_id.is_none() {
                prompts += 1;
            }
            continue;
        }

        steps += 1;
        add_tokens(&mut tokens, &message.data);
        cost += float_at(&message.data, "/cost");

        let ordinal = day_ordinal(message.time);
        let entry = activity
            .entry(ordinal)
            .or_insert_with(|| (date_key(message.time), 0));
        entry.1 += 1;

        let provider = message
            .data
            .pointer("/model/providerID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let model_id = message
            .data
            .pointer("/model/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !provider.is_empty() && !model_id.is_empty() {
            let variant = message
                .data
                .pointer("/model/variant")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = format!("{provider}/{model_id}#{variant}");
            let entry = models.entry(key).or_insert_with(|| {
                let mut reference = Map::new();
                reference.insert("providerID".to_owned(), json!(provider));
                reference.insert("id".to_owned(), json!(model_id));
                if !variant.is_empty() {
                    reference.insert("variant".to_owned(), json!(variant));
                }
                (Value::Object(reference), 0, empty_tokens(), 0.0)
            });
            entry.1 += 1;
            add_tokens(&mut entry.2, &message.data);
            entry.3 += float_at(&message.data, "/cost");
        }

        if tools_mode != "none"
            && let Some(content) = message.data.get("content").and_then(Value::as_array)
        {
            for part in content {
                if part.get("type").and_then(Value::as_str) != Some("tool") {
                    continue;
                }
                let name = part
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let status = part
                    .pointer("/state/status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let entry = tools.entry(name).or_default();
                entry.calls += 1;
                match status {
                    "completed" => entry.succeeded += 1,
                    "error" => entry.failed += 1,
                    _ => entry.unfinished += 1,
                }
                let ran = part.pointer("/time/ran").and_then(Value::as_i64);
                let completed = part.pointer("/time/completed").and_then(Value::as_i64);
                if let (Some(ran), Some(completed)) = (ran, completed)
                    && completed >= ran
                    && let Ok(duration) = u32::try_from(completed - ran)
                {
                    entry.durations.push(duration);
                }
            }
        }
    }

    let tool_totals = tools.values().fold(
        (0_i64, 0_i64, 0_i64, 0_i64),
        |(calls, succeeded, failed, unfinished), entry| {
            (
                calls + entry.calls,
                succeeded + entry.succeeded,
                failed + entry.failed,
                unfinished + entry.unfinished,
            )
        },
    );

    let tool_value = match tools_mode {
        "none" => json!({ "mode": "none" }),
        "detail" => {
            let mut usage: Vec<(String, &ToolAggregate)> = tools
                .iter()
                .map(|(name, entry)| (name.clone(), entry))
                .collect();
            usage.sort_by_key(|(_, entry)| Reverse(entry.calls));
            let usage: Vec<Value> = usage
                .into_iter()
                .map(|(name, entry)| {
                    let mut durations = entry.durations.clone();
                    json!({
                        "name": name,
                        "calls": entry.calls,
                        "succeeded": entry.succeeded,
                        "failed": entry.failed,
                        "unfinished": entry.unfinished,
                        "durationP50": median(&mut durations),
                    })
                })
                .collect();
            json!({
                "mode": "detail",
                "totals": {
                    "calls": tool_totals.0,
                    "succeeded": tool_totals.1,
                    "failed": tool_totals.2,
                    "unfinished": tool_totals.3,
                },
                "usage": usage,
            })
        }
        _ => json!({
            "mode": "summary",
            "totals": {
                "calls": tool_totals.0,
                "succeeded": tool_totals.1,
                "failed": tool_totals.2,
                "unfinished": tool_totals.3,
            },
        }),
    };

    let days: Vec<(String, i64)> = activity.into_values().collect();
    let streak = days
        .iter()
        .enumerate()
        .fold((0_i64, 0_i64), |(current, longest), (index, (date, _))| {
            let ordinal = day_ordinal_from_key(date);
            let previous = index
                .checked_sub(1)
                .and_then(|previous| days.get(previous))
                .map(|(date, _)| day_ordinal_from_key(date));
            let current = match previous {
                Some(previous) if ordinal - previous == 1 => current + 1,
                _ => 1,
            };
            (current, longest.max(current))
        })
        .1;
    let active_days = i64::try_from(days.len()).unwrap_or(i64::MAX);

    let mut models: Vec<(i64, Value)> = models
        .into_values()
        .map(|(model, steps, tokens, cost)| {
            (
                tokens_total(&tokens),
                json!({
                    "model": model,
                    "steps": steps,
                    "tokens": tokens,
                    "cost": cost,
                }),
            )
        })
        .collect();
    models.sort_by_key(|(total, _)| Reverse(*total));

    Ok(Json(json!({
        "data": {
            "range": { "from": from, "to": to },
            "sessions": unique_sessions.len(),
            "subagents": subagents.len(),
            "prompts": prompts,
            "steps": steps,
            "tokens": tokens,
            "cost": cost,
            "tools": tool_value,
            "activeDays": active_days,
            "streak": streak,
            "activity": days
                .into_iter()
                .map(|(date, steps)| json!({ "date": date, "steps": steps }))
                .collect::<Vec<_>>(),
            "models": models.into_iter().map(|(_, value)| value).collect::<Vec<_>>(),
        }
    })))
}

fn day_ordinal_from_key(date: &str) -> i64 {
    let mut parts = date.split('-');
    let year: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    let month: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    let day: i64 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    days_from_civil(year, month, day)
}

/// Inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------------------
// Skill activation
// ---------------------------------------------------------------------------

/// `POST /api/experimental/session/:sid/skill`
#[derive(Debug, Deserialize)]
pub struct SkillBody {
    id: String,
    #[serde(default)]
    resume: Option<bool>,
}

/// `POST /api/experimental/session/:sid/skill`
pub async fn skill(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<SkillBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let (skill_name, skill_id, skill_content) = {
        let config = require_config(&state)?;
        let skill = config.skill(&body.id).ok_or_else(|| {
            ApiError::not_found(
                "SkillNotFoundError",
                format!("Skill not found: {}", body.id),
            )
            .with("skill", json!(body.id))
        })?;
        (skill.name.clone(), skill.id.clone(), skill.content.clone())
    };
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;

    // The runtime owns loaded skill state; a synthetic inbox item carrying the
    // skill body is the durable hand-off it consumes like any other prompt.
    let text = format!(
        "<skill_content name=\"{skill_name}\">\n{}\n</skill_content>",
        skill_content.trim_end()
    );
    let id = MessageId::new().into_inner();
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.clone(),
            kind: "synthetic".to_owned(),
            payload: json!({
                "text": text,
                "description": format!("Activate skill {skill_id}"),
            }),
            delivery: Delivery::Queue,
        })
        .await?;
    publish_enqueued(&state, &session_id, &id, &item).await?;
    if body.resume != Some(false) {
        notify(&state, &session_id).await?;
    }
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// Instruction entries
// ---------------------------------------------------------------------------

/// `PUT /api/experimental/session/:sid/instructions/entries/:key`
#[derive(Debug, Deserialize)]
pub struct InstructionBody {
    value: Value,
}

fn valid_instruction_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
}

fn invalid_instruction_key(key: &str) -> ApiError {
    ApiError::new(
        400,
        "InvalidRequestError",
        format!("Invalid instruction entry key {key:?}"),
    )
}

/// `GET /api/experimental/session/:sid/instructions/entries`
pub async fn instruction_list(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Value>, ErrorResponse> {
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    let entries = store.instruction_entries(&session_id).await?;
    Ok(Json(json!({ "data": entries })))
}

/// `PUT /api/experimental/session/:sid/instructions/entries/:key`
pub async fn instruction_put(
    Path((session_id, key)): Path<(String, String)>,
    State(state): State<AppState>,
    body: Result<Json<InstructionBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if !valid_instruction_key(&key) {
        return Err(invalid_instruction_key(&key).into());
    }
    let encoded = serde_json::to_string(&body.value)
        .map_err(|_| ErrorResponse::from(ApiError::internal("Internal error")))?;
    let actual = encoded.len();
    if actual > INSTRUCTION_MAX_BYTES {
        return Err(ApiError::new(
            413,
            "InstructionEntryValueTooLargeError",
            format!("instruction entry is {actual} bytes; the maximum is {INSTRUCTION_MAX_BYTES}"),
        )
        .with("actualBytes", json!(actual))
        .with("maxBytes", json!(INSTRUCTION_MAX_BYTES))
        .into());
    }
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    store
        .set_instruction_entry(&session_id, &key, Some(encoded.as_str()))
        .await?;
    Ok(no_content())
}

/// `DELETE /api/experimental/session/:sid/instructions/entries/:key`
pub async fn instruction_delete(
    Path((session_id, key)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    if !valid_instruction_key(&key) {
        return Err(invalid_instruction_key(&key).into());
    }
    let store = require_store(&state)?;
    require_session(store, &session_id).await?;
    store.set_instruction_entry(&session_id, &key, None).await?;
    Ok(no_content())
}
