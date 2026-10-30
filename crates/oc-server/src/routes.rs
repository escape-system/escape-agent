//! Handlers for the phase 0 slice.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, RawQuery, Request, State};
use axum::http::header::{
    ACCEPT, ALLOW, CACHE_CONTROL, LOCATION, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use oc_types::{ApiError, EventEnvelope, LocationRef, ServerInfo};
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;
use crate::data::location;
use crate::data::query_error;
use crate::data::session::LocationQuery;
use crate::error::{ErrorResponse, Respond};

/// `GET /api/info`
pub async fn info(State(state): State<AppState>) -> Json<ServerInfo> {
    Json(state.info().clone())
}

/// `GET /api/location` — the bare `Location.PublicInfo` value.
pub async fn location(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationRef>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    if !resolved.path().is_dir() {
        return Err(ApiError::not_found(
            "LocationNotFoundError",
            format!("Location not found: {}", resolved.directory),
        )
        .with("location", json!({ "directory": resolved.directory }))
        .into());
    }
    if let Some(store) = state.store() {
        let reference = resolved.reference();
        store
            .register_project(
                reference.project.id.as_str(),
                &reference.project.directory,
                &reference.project.canonical,
            )
            .await
            .map_err(ErrorResponse::from)?;
    }
    Ok(Json(resolved.reference()))
}

/// `POST /api/pair` — authenticated; the code it returns mints a session token.
pub async fn pair(State(state): State<AppState>) -> Json<Value> {
    let code = state.pairing().issue();
    Json(json!({"code": code, "expires_in": 300}))
}

/// `GET /auth/connect/:code` — public; JSON clients receive a session token,
/// browsers receive a cookie and a redirect.
pub async fn connect(
    Path(code): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !state.pairing().redeem(&code) {
        return ApiError::new(
            StatusCode::UNAUTHORIZED.as_u16(),
            "UnauthorizedError",
            "Invalid or expired pairing code",
        )
        .respond();
    }

    let wants_html = headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));

    if wants_html {
        let mut builder = Response::builder()
            .status(StatusCode::FOUND)
            .header(LOCATION, "/")
            .header(CACHE_CONTROL, "no-store");
        if let Some(password) = state.password()
            && state.port() != 0
        {
            let token = crate::session_token::issue(password);
            let cookie = format!(
                "opencode_session_{}={token}; HttpOnly; SameSite=Lax; Max-Age={}; Path=/",
                state.port(),
                crate::session_token::TTL_SECONDS
            );
            builder = builder.header(SET_COOKIE, cookie);
        }
        return builder
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    // Browsers get a cookie above; API clients get the session token the
    // cookie would have carried. Empty only when no password is configured.
    let token = state
        .password()
        .map(crate::session_token::issue)
        .unwrap_or_default();

    let mut response = Json(json!({ "token": token })).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `GET /api/event` — validate the SSE contract: exact headers, a mandatory
/// `server.connected` first frame, then live events filtered by `?types=`,
/// with a 15 s comment heartbeat while idle. A lagged subscriber (broadcast
/// overflow) ends its stream instead of silently skipping events.
pub async fn events(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let receiver = state.bus().subscribe();
    let types = Arc::new(event_types(query.as_deref()));

    let first = futures_util::stream::once(async {
        Ok::<Event, Infallible>(Event::default().data(EventEnvelope::connected().to_sse_data()))
    });
    let live = BroadcastStream::new(receiver)
        // The queue overflowed and events were dropped: end this stream. The
        // contract is a close, never a silent gap.
        .take_while(|item| {
            let keep = item.is_ok();
            async move { keep }
        })
        .filter_map(move |item| {
            let types = Arc::clone(&types);
            async move {
                let event = item.ok()?;
                if !types.is_empty() && !types.contains(event.kind.as_str()) {
                    return None;
                }
                Some(Ok::<Event, Infallible>(
                    Event::default().data(event.to_sse_data()),
                ))
            }
        });

    let stream = first.chain(live);
    let mut response = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("heartbeat"),
        )
        .into_response();

    let headers = response.headers_mut();
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

/// Parse `?types=a&types=b,c` into the filter set (empty means "all").
fn event_types(raw: Option<&str>) -> HashSet<String> {
    let mut types = HashSet::new();
    let Some(raw) = raw else {
        return types;
    };
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if key != "types" {
            continue;
        }
        for value in value.split(',') {
            let value = value.trim();
            if !value.is_empty() {
                types.insert(value.to_owned());
            }
        }
    }
    types
}

/// Everything not matched: API 404s are JSON, page routes are the SPA
/// fallback, and a missing web build is reported honestly.
pub async fn fallback(req: Request) -> Response {
    let path = req.uri().path().to_owned();
    let state = req.extensions().get::<AppState>().cloned();

    if path.starts_with("/api") || path.starts_with("/auth") || path == "/openapi.json" {
        return ApiError::not_found("NotFoundError", format!("Route not found: {path}")).respond();
    }

    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let mut response = ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED.as_u16(),
            "MethodNotAllowedError",
            "Method not allowed",
        )
        .respond();
        let headers = response.headers_mut();
        headers.insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }

    let method = req.method().clone();
    let uri_path = req.uri().path().to_owned();
    if let Some(state) = state
        && let Some(response) = state.static_response(&method, &uri_path).await
    {
        return response;
    }

    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        "Web UI is not built. Run ui/build.sh (or set OC_WEB_DIST) and restart.\n",
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
