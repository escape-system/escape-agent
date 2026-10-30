//! `GET /api/session/:sid/log` — the only resumable SSE stream.
//!
//! The subscription is established before the historical read. Events that
//! commit during the replay therefore arrive live and are dropped by sequence,
//! so each durable event reaches the client exactly once, in order, followed by
//! a `log.synced` marker.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::HeaderName;
use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use oc_types::{ApiError, EventEnvelope};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;
use crate::data::events::from_stored;
use crate::data::{query_error, require_store, session_not_found};
use crate::error::ErrorResponse;

/// `GET /api/session/:sid/log`
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogParams {
    after: Option<String>,
    follow: Option<bool>,
}

/// Durable replay followed by live events for one session.
pub async fn stream(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    params: Result<Query<LogParams>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let after = match params.after.as_deref() {
        None | Some("") => 0,
        Some(raw) => raw.parse::<i64>().map_err(|_| {
            ErrorResponse::from(ApiError::new(
                400,
                "BadRequestError",
                format!("Invalid after sequence: {raw:?}"),
            ))
        })?,
    };
    let follow = params.follow.unwrap_or(false);

    let store = require_store(&state)?;
    let session = store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map_or_else(
            || state.directory().to_string_lossy().into_owned(),
            str::to_owned,
        );

    // Subscribe before reading so a commit during replay is not lost.
    let receiver = state.bus().subscribe();

    let mut envelopes: Vec<EventEnvelope> = Vec::new();
    let mut cursor = after;
    loop {
        let batch = store.events_after(&session_id, cursor, 1_000).await?;
        if batch.is_empty() {
            break;
        }
        let count = batch.len();
        if let Some(last) = batch.last() {
            cursor = last.seq;
        }
        envelopes.extend(
            batch
                .into_iter()
                .map(|event| from_stored(event, &directory)),
        );
        if count < 1_000 {
            break;
        }
    }

    let mut frames: Vec<Result<Event, Infallible>> = envelopes
        .into_iter()
        .map(|envelope| Ok(Event::default().data(envelope.to_sse_data())))
        .collect();

    let marker = if cursor == 0 && after == 0 {
        json!({ "type": "log.synced", "aggregateID": session_id })
    } else {
        json!({ "type": "log.synced", "aggregateID": session_id, "seq": cursor })
    };
    frames.push(Ok(Event::default().data(marker.to_string())));

    let live: BoxStream<'static, Result<Event, Infallible>> = if follow {
        let cursor = Arc::new(AtomicI64::new(cursor));
        let filter_session = session_id.clone();
        Box::pin(BroadcastStream::new(receiver).filter_map(move |item| {
            let cursor = Arc::clone(&cursor);
            let filter_session = filter_session.clone();
            async move {
                let event = item.ok()?;
                let durable = event.durable.as_ref()?;
                if durable.aggregate_id != filter_session {
                    return None;
                }
                if durable.seq <= cursor.load(Ordering::Relaxed) {
                    return None;
                }
                cursor.store(durable.seq, Ordering::Relaxed);
                Some(Ok(Event::default().data(event.to_sse_data())))
            }
        }))
    } else {
        Box::pin(stream::empty())
    };

    let mut response = Sse::new(stream::iter(frames).chain(live))
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
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    Ok(response)
}
