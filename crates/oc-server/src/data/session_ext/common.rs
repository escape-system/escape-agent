//! Shared store readers, error constructors and JSON accessors used by the
//! fork, revert, context, diff, view, forms, export, import and stats handlers.

use std::collections::HashMap;

use oc_store::{MessageQuery, Order, SessionQuery, Store, StoreError};
use oc_types::ApiError;
use serde_json::{Value, json};

use crate::data::session_not_found;
use crate::error::ErrorResponse;

/// Page size used by every full-scan helper (the store clamps to 200).
const PAGE: usize = 200;

/// First value beyond `i64::MAX` that is representable as `f64` (2^63).
const I64_F64_LIMIT: f64 = 9_223_372_036_854_775_808.0;

/// Loads a session or answers `SessionNotFoundError`.
pub(super) async fn require_session(
    store: &Store,
    session_id: &str,
) -> Result<Value, ErrorResponse> {
    store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id).into())
}

/// Reads every message of a session in ascending `seq` order.
pub(super) async fn messages_all(
    store: &Store,
    session_id: &str,
) -> Result<Vec<Value>, StoreError> {
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
pub(super) async fn sessions_all(
    store: &Store,
    project: Option<&str>,
) -> Result<Vec<Value>, StoreError> {
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

pub(super) fn message_not_found(session_id: &str, message_id: &str) -> ApiError {
    ApiError::not_found(
        "MessageNotFoundError",
        format!("Message not found: {message_id}"),
    )
    .with("sessionID", json!(session_id))
    .with("messageID", json!(message_id))
}

/// Truncates a finite float to `i64` without a lossy `as` cast, saturating at
/// the integer bounds exactly like a numeric cast would.
pub(super) fn float_i64(number: f64) -> Option<i64> {
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
pub(super) fn json_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_u64()
            .and_then(|number| i64::try_from(number).ok())
            .or_else(|| value.as_f64().and_then(float_i64))
    })
}

pub(super) fn time_of(value: &Value) -> Option<i64> {
    value.pointer("/time/created").and_then(json_i64)
}

pub(super) fn id_of(value: &Value) -> String {
    value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The stored message body: identity fields belong to the row, not the body.
pub(super) fn message_body(message: &Value) -> Value {
    let mut map = message.as_object().cloned().unwrap_or_default();
    map.remove("id");
    map.remove("sessionID");
    map.remove("type");
    Value::Object(map)
}

/// Rewrites a copied compaction row's `keepID` to the remapped identity.
///
/// Compaction rows keep `keepID` at the top level of the message body (what
/// `turn/compaction.rs` writes); older rows nested it under `metadata`. The
/// nested key is read as a fallback and dropped when the reference is
/// remapped, so a copied row never carries a stale identity anywhere.
pub(super) fn remap_keep_id(data: &mut Value, identities: &HashMap<String, String>) {
    let kept = data
        .get("keepID")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/metadata/keepID").and_then(Value::as_str))
        .and_then(|kept| identities.get(kept))
        .cloned();
    let Some(kept) = kept else {
        return;
    };
    if let Some(metadata) = data.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("keepID");
    }
    data["keepID"] = Value::String(kept);
}

pub(super) fn non_null(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| !value.is_null()).cloned()
}
