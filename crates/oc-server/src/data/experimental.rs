//! Experimental and debug routes that are not tied to a runtime service.
//!
//! These handlers are deliberately honest: features that need a service the
//! server does not have (media blobs, model generation, client MCP tunnels,
//! heap dumps, the v1 migration scanner) answer a documented `503`, `501` or
//! `404` instead of fabricating success. The two routes with real behavior are
//! the filesystem write (atomic, confined to the request location) and the
//! filesystem watch registry, which polls registered paths and publishes
//! `filesystem.changed` only when the file actually changed.
//!
//! `POST /api/location/reload` can only rescan skills: `ConfigService` is an
//! immutable snapshot, so the route never claims a full configuration reload.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Extension, Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use oc_types::{ApiError, EventEnvelope, EventLocation, LocationEnvelope, LocationRef};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;

use crate::AppState;
use crate::data::fs;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error, schema_error};
use crate::error::ErrorResponse;

/// Largest raw body `fs/write` accepts.
const MAX_WRITE_BYTES: usize = 64 * 1024 * 1024;
/// One poll tick when the integrator uses [`routes`].
const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Upper bound on concurrent watches per location (mirrors upstream).
const MAX_WATCHES: usize = 256;
/// Upper bound on watch tasks across the process, so many locations cannot
/// multiply the per-location limit without bound.
const MAX_TOTAL_WATCHES: usize = 1024;
/// Header fallback for `fs/write`'s `path` query parameter.
const PATH_HEADER: &str = "x-opencode-path";

/// `data::experimental` route table.
///
/// The caller merges this into the protected router; it is not an
/// `AppState`-owning router by itself.
pub fn routes() -> Router<AppState> {
    routes_with_watch_interval(DEFAULT_WATCH_INTERVAL)
}

/// Route table with a caller-chosen watch poll interval (tests use a short
/// interval; production uses [`DEFAULT_WATCH_INTERVAL`]).
pub fn routes_with_watch_interval(interval: Duration) -> Router<AppState> {
    let interval = interval.max(Duration::from_millis(10));
    Router::new()
        .route("/api/location/reload", post(reload_location))
        .route("/api/experimental/fs/write", post(write_file))
        .route(
            "/api/experimental/fs/watch",
            post(watch_file).delete(unwatch_file),
        )
        .route("/api/experimental/media/{hash}", get(media))
        .route("/api/experimental/client-mcp/token", post(client_mcp_token))
        .route("/api/experimental/generate", post(generate_text))
        .route("/api/experimental/generate/text", post(generate_text))
        .route("/api/experimental/generate/image", post(generate_image))
        .route(
            "/api/experimental/generate/decisions",
            post(generate_decisions),
        )
        .route("/api/session/{sid}/generate", post(generate_session_text))
        .route(
            "/api/debug/location",
            get(debug_location).delete(evict_location),
        )
        .route("/api/debug/heap-dump", post(heap_dump))
        .route("/api/experimental/migration/v1", get(migration_status))
        .layer(Extension(WatchConfig { interval }))
        .layer(DefaultBodyLimit::max(MAX_WRITE_BYTES))
}

/// Interval handed to the watch handlers through an extension layer.
#[derive(Debug, Clone, Copy)]
struct WatchConfig {
    interval: Duration,
}

/// `POST /api/location/reload`
///
/// The configuration is an immutable snapshot, so the best truthful action is
/// to rediscover skills; instructions cannot be reloaded through the current
/// [`oc_config::ConfigService`] API. A missing configuration service is the
/// documented `503` rather than a fake `204`.
async fn reload_location(State(state): State<AppState>) -> Result<Response, ErrorResponse> {
    let config = state.config().ok_or_else(|| {
        ErrorResponse::from(ApiError::service_unavailable(
            "configuration service not available; skills were not rescanned",
        ))
    })?;
    let _skills = config.rescan_skills();
    Ok(no_content())
}

/// `POST /api/experimental/fs/write`
///
/// The raw body is written atomically (temporary file plus rename) after
/// resolving `path` inside the request location. Missing parent directories
/// are created; `..` and symlink escapes are rejected.
async fn write_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<WriteQuery>, QueryRejection>,
    body: Body,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let raw_path = query_path(&headers, &params)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let root = fs::canonical_root(&resolved)?;
    let target = fs::confined_target(&root, &raw_path)?;
    if tokio::fs::metadata(&target)
        .await
        .is_ok_and(|meta| meta.is_dir())
    {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "IsDirectoryError",
            format!("Path is a directory: {raw_path}"),
        )));
    }
    let parent = target.parent().ok_or_else(|| {
        ErrorResponse::from(ApiError::new(
            400,
            "BadRequestError",
            "A file path is required",
        ))
    })?;
    let file_name = target.file_name().ok_or_else(|| {
        ErrorResponse::from(ApiError::new(
            400,
            "BadRequestError",
            "A file path is required",
        ))
    })?;

    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| ErrorResponse::from(fs::internal(error)))?;
    // Directory creation happens after the first check; repeat it on the
    // canonical parent so a concurrently swapped symlink cannot escape.
    let canonical_parent = tokio::fs::canonicalize(parent)
        .await
        .map_err(|error| ErrorResponse::from(fs::internal(error)))?;
    if !canonical_parent.starts_with(&root) {
        return Err(ErrorResponse::from(fs::file_not_found(&raw_path)));
    }
    let target = canonical_parent.join(file_name);

    let bytes = to_bytes(body, MAX_WRITE_BYTES).await.map_err(|_| {
        ErrorResponse::from(ApiError::new(
            413,
            "RequestTooLargeError",
            format!("Request body exceeds the {MAX_WRITE_BYTES}-byte write limit"),
        ))
    })?;
    write_atomically(&target, &bytes)
        .await
        .map_err(|error| ErrorResponse::from(fs::internal(error)))?;

    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        json!({"path": fs::display_path(&target)}),
    )))
}

/// `POST /api/experimental/fs/watch`
///
/// Registers a polling watcher for one file inside the request location. The
/// registry is process-wide and bounded per location; each registration owns
/// exactly one task, cancelled by `DELETE` or by eviction.
async fn watch_file(
    State(state): State<AppState>,
    Extension(config): Extension<WatchConfig>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    payload: Result<Json<WatchBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(body) = payload.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let root = fs::canonical_root(&resolved)?;
    let target = fs::confined(&root, &body.path)?;
    let meta = tokio::fs::metadata(&target)
        .await
        .map_err(|_| ErrorResponse::from(fs::file_not_found(&body.path)))?;
    if !meta.is_file() {
        return Err(ErrorResponse::from(fs::file_not_found(&body.path)));
    }

    let key = WatchKey {
        location: resolved.directory.clone(),
        file: target.clone(),
    };
    register(
        key,
        config.interval,
        state.bus().clone(),
        resolved.event(),
        fs::display_path(&target),
    );
    Ok(no_content())
}

/// `DELETE /api/experimental/fs/watch`
async fn unwatch_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
    payload: Result<Json<WatchBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let Json(body) = payload.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let root = fs::canonical_root(&resolved)?;
    let target = fs::confined_target(&root, &body.path)?;
    let key = WatchKey {
        location: resolved.directory.clone(),
        file: target,
    };
    let entry = lock_registry().entries.remove(&key);
    if let Some(entry) = entry {
        entry.handle.abort();
    }
    Ok(no_content())
}

/// `GET /api/experimental/media/{hash}`
///
/// Media blobs live in the store's session-blob table upstream; this server
/// has no blob API, so no hash can be served. Answering `404` is the truthful
/// result, and the route exists so clients fail fast instead of retrying.
async fn media(AxumPath(hash): AxumPath<String>) -> ErrorResponse {
    ApiError::not_found(
        "FileNotFoundError",
        format!("Media blob not found: {hash} (this server has no media blob store)"),
    )
    .with("path", json!(format!("/api/experimental/media/{hash}")))
    .into()
}

/// `POST /api/experimental/client-mcp/token`
///
/// The client MCP tunnel (`GET /api/experimental/client-mcp`) is not
/// implemented; minting a ticket would hand out an unusable credential.
async fn client_mcp_token() -> ErrorResponse {
    ApiError::service_unavailable("client MCP tunnel not available").into()
}

/// `POST /api/experimental/generate` and `/generate/text`
async fn generate_text(payload: Result<Json<Value>, JsonRejection>) -> ErrorResponse {
    unavailable_generation(
        payload,
        validate_text_body,
        "text generation service not attached",
    )
}

/// `POST /api/experimental/generate/image`
async fn generate_image(payload: Result<Json<Value>, JsonRejection>) -> ErrorResponse {
    unavailable_generation(
        payload,
        validate_image_body,
        "image generation service not attached",
    )
}

/// `POST /api/experimental/generate/decisions`
async fn generate_decisions(payload: Result<Json<Value>, JsonRejection>) -> ErrorResponse {
    unavailable_generation(
        payload,
        validate_decisions_body,
        "decision generation service not attached",
    )
}

/// `POST /api/session/{sid}/generate`
async fn generate_session_text(payload: Result<Json<Value>, JsonRejection>) -> ErrorResponse {
    unavailable_generation(
        payload,
        validate_session_body,
        "session text generation service not attached",
    )
}

/// Validate the documented body and then report the missing generator. Schema
/// errors win over the `503`, so malformed requests are never reported as a
/// transient unavailability.
fn unavailable_generation(
    payload: Result<Json<Value>, JsonRejection>,
    validate: fn(&Value) -> Result<(), String>,
    message: &'static str,
) -> ErrorResponse {
    let Json(value) = match payload {
        Ok(payload) => payload,
        Err(rejection) => return ErrorResponse::from(schema_error(rejection)),
    };
    match validate(&value) {
        Ok(()) => ErrorResponse::from(ApiError::service_unavailable(message)),
        Err(error) => ErrorResponse::from(schema_error(error)),
    }
}

/// `GET /api/debug/location`
async fn debug_location(State(state): State<AppState>, headers: HeaderMap) -> Json<LocationRef> {
    Json(location::resolve(&state, &headers, None, None).reference())
}

/// `DELETE /api/debug/location`
///
/// Locations are resolved per request; there is no cache to evict. Reporting
/// success would pretend a state change happened.
async fn evict_location() -> ErrorResponse {
    ApiError::new(
        501,
        "NotImplementedError",
        "there is no location cache to evict; locations are resolved per request",
    )
    .into()
}

/// `POST /api/debug/heap-dump`
async fn heap_dump() -> ErrorResponse {
    ApiError::new(
        501,
        "NotImplementedError",
        "heap dumps are not implemented for this native server",
    )
    .into()
}

/// `GET /api/experimental/migration/v1`
///
/// Reading v1 data is out of scope (docs/16-roadmap.md); no migration state is
/// invented.
async fn migration_status() -> ErrorResponse {
    ApiError::new(
        501,
        "NotImplementedError",
        "the legacy v1 migration scan is not implemented; this server reads no v1 data",
    )
    .into()
}

/// `fs/write` query: `path` plus the shared deep-object location.
#[derive(Debug, Deserialize)]
struct WriteQuery {
    path: Option<String>,
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
}

/// Resolve the write path from the query, then the `x-opencode-path` header.
fn query_path(headers: &HeaderMap, params: &WriteQuery) -> Result<String, ErrorResponse> {
    params
        .path
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            headers
                .get(PATH_HEADER)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            ErrorResponse::from(ApiError::new(
                400,
                "BadRequestError",
                "A file path is required: pass ?path= or the x-opencode-path header",
            ))
        })
}

/// `fs/watch` body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchBody {
    path: String,
}

/// The request body must be a JSON object.
fn body_object(value: &Value) -> Result<&Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "the request body must be a JSON object".to_owned())
}

/// A required string field.
fn string_field<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{name}` must be a string"))
}

/// `Model.Ref` requires `id` and `providerID`; `variant` is an optional string.
fn validate_model(value: &Value) -> Result<(), String> {
    let model = value
        .as_object()
        .ok_or_else(|| "`model` must be an object".to_owned())?;
    string_field(model, "id")?;
    string_field(model, "providerID")?;
    if let Some(variant) = model.get("variant")
        && !variant.is_string()
    {
        return Err("`model.variant` must be a string".to_owned());
    }
    Ok(())
}

fn validate_optional_model(object: &Map<String, Value>) -> Result<(), String> {
    match object.get("model") {
        None | Some(Value::Null) => Ok(()),
        Some(model) => validate_model(model),
    }
}

fn validate_text_body(value: &Value) -> Result<(), String> {
    let object = body_object(value)?;
    string_field(object, "prompt")?;
    validate_optional_model(object)
}

fn validate_image_body(value: &Value) -> Result<(), String> {
    let object = body_object(value)?;
    let model = object
        .get("model")
        .ok_or_else(|| "`model` is required".to_owned())?;
    validate_model(model)?;
    string_field(object, "prompt")?;
    if let Some(images) = object.get("images")
        && !images.is_array()
    {
        return Err("`images` must be an array".to_owned());
    }
    if let Some(n) = object.get("n")
        && n.as_u64().is_none_or(|n| n == 0)
    {
        return Err("`n` must be a positive integer".to_owned());
    }
    if let Some(aspect) = object.get("aspect")
        && !aspect.is_string()
    {
        return Err("`aspect` must be a string".to_owned());
    }
    if let Some(resolution) = object.get("resolution")
        && !resolution.is_string()
    {
        return Err("`resolution` must be a string".to_owned());
    }
    Ok(())
}

fn validate_decisions_body(value: &Value) -> Result<(), String> {
    let object = body_object(value)?;
    let model = object
        .get("model")
        .ok_or_else(|| "`model` is required".to_owned())?;
    validate_model(model)?;
    let input = object
        .get("input")
        .ok_or_else(|| "`input` is required".to_owned())?;
    if !input.is_array() {
        return Err("`input` must be an array".to_owned());
    }
    let questions = object
        .get("questions")
        .ok_or_else(|| "`questions` is required".to_owned())?;
    let Some(questions) = questions.as_array() else {
        return Err("`questions` must be an array".to_owned());
    };
    if questions.is_empty() {
        return Err("at least one decision question is required".to_owned());
    }
    if questions.iter().any(|question| !question.is_object()) {
        return Err("each decision question must be an object".to_owned());
    }
    Ok(())
}

fn validate_session_body(value: &Value) -> Result<(), String> {
    let object = body_object(value)?;
    string_field(object, "prompt")?;
    Ok(())
}

/// One watcher registration, keyed by location and canonical file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WatchKey {
    location: String,
    file: PathBuf,
}

/// The task plus its registration order, used for bounded eviction.
#[derive(Debug)]
struct WatchEntry {
    sequence: u64,
    handle: tokio::task::JoinHandle<()>,
}

/// Process-wide watch registry (upstream keeps its watcher global too).
#[derive(Debug, Default)]
struct WatchRegistry {
    entries: HashMap<WatchKey, WatchEntry>,
    sequence: u64,
}

fn registry() -> &'static Mutex<WatchRegistry> {
    static REGISTRY: LazyLock<Mutex<WatchRegistry>> =
        LazyLock::new(|| Mutex::new(WatchRegistry::default()));
    &REGISTRY
}

fn lock_registry() -> std::sync::MutexGuard<'static, WatchRegistry> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Register (or refresh) a watch, evicting the oldest registration for this
/// location when the per-location bound is reached.
fn register(
    key: WatchKey,
    interval: Duration,
    bus: broadcast::Sender<Arc<EventEnvelope>>,
    location: EventLocation,
    file: String,
) {
    let target = key.file.clone();
    let mut registry = lock_registry();
    registry.sequence += 1;
    let sequence = registry.sequence;
    if let Some(entry) = registry.entries.get_mut(&key) {
        entry.sequence = sequence;
        return;
    }

    let same_location = registry
        .entries
        .iter()
        .filter(|(candidate, _)| candidate.location == key.location)
        .count();
    let over_total = registry.entries.len() >= MAX_TOTAL_WATCHES;
    if over_total || same_location >= MAX_WATCHES {
        let oldest = registry
            .entries
            .iter()
            .filter(|(candidate, _)| over_total || candidate.location == key.location)
            .min_by_key(|(_, entry)| entry.sequence)
            .map(|(candidate, _)| candidate.clone());
        if let Some(oldest) = oldest
            && let Some(entry) = registry.entries.remove(&oldest)
        {
            entry.handle.abort();
        }
    }

    let initial = signature_now(&target);
    let handle = tokio::spawn(poll_watch(interval, bus, location, file, target, initial));
    registry
        .entries
        .insert(key, WatchEntry { sequence, handle });
}

/// Poll one file until the task is aborted, publishing on real transitions.
async fn poll_watch(
    interval: Duration,
    bus: broadcast::Sender<Arc<EventEnvelope>>,
    location: EventLocation,
    file: String,
    target: PathBuf,
    initial: Option<(u64, u64)>,
) {
    let mut last = initial;
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; the baseline was taken at registration.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let current = signature(&target).await;
        if current == last {
            continue;
        }
        let event = match (last, current) {
            (Some(_), None) => "unlink",
            (None, Some(_)) => "add",
            _ => "change",
        };
        let mut envelope =
            EventEnvelope::now("filesystem.changed", json!({"file": file, "event": event}));
        envelope.location = Some(location.clone());
        let _ = bus.send(Arc::new(envelope));
        last = current;
    }
}

/// `(mtime in milliseconds, size)` used to detect a file transition.
async fn signature(path: &Path) -> Option<(u64, u64)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    signature_of(&meta)
}

fn signature_now(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    signature_of(&meta)
}

fn signature_of(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    if !meta.is_file() {
        return None;
    }
    let modified = meta.modified().ok()?;
    let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
    Some((u64::try_from(millis).unwrap_or(u64::MAX), meta.len()))
}

/// Write `data` to a temporary sibling and rename it over `target`.
async fn write_atomically(target: &Path, data: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = target.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "target has no parent")
    })?;
    let name = target.file_name().map_or_else(
        || "file".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    let result = async {
        let mut file = tokio::fs::File::create(&temp).await?;
        file.write_all(data).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, target).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}
