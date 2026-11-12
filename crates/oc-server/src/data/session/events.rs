//! Durable publish helpers: runtime notify, inbox-enqueued event and the
//! session-created payload.

use oc_types::{ApiError, EventPayload, InboxItem, MessageId, SessionId, SessionInboxEnqueued};
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::runtime_error;
use crate::error::ErrorResponse;

/// Wake the runtime for a session that just received input.
pub(crate) async fn notify(state: &AppState, session_id: &str) -> Result<(), ErrorResponse> {
    if let Some(runtime) = state.runtime() {
        runtime.notify(session_id).await.map_err(runtime_error)?;
    }
    Ok(())
}

pub(crate) async fn publish_enqueued(
    state: &AppState,
    session_id: &str,
    id: &str,
    item: &Value,
) -> Result<(), ErrorResponse> {
    let item: InboxItem = serde_json::from_value(item.clone()).map_err(|error| {
        tracing::error!(error = %error, "inbox row is not a wire item");
        ErrorResponse::from(ApiError::internal("Internal error"))
    })?;
    let payload = SessionInboxEnqueued {
        session_id: SessionId::from_wire(session_id),
        inbox_id: MessageId::from_wire(id),
        item,
    };
    publish_durable(
        state,
        session_id,
        SessionInboxEnqueued::TYPE,
        payload.data(),
        SessionInboxEnqueued::VERSION,
    )
    .await?;
    Ok(())
}

pub(super) fn created_event_data(session: &Value, version: &str) -> Value {
    // Identity is guaranteed by the schema, not by this function:
    // `session_v2.id` is a primary key, `session_v2.project_id` is
    // `TEXT NOT NULL REFERENCES project(id)` (schema.rs:41), and
    // `SESSION_COLUMNS` coalesces the nullable `session_v2.directory` onto
    // `project.directory`, which is `TEXT NOT NULL` (schema.rs:18). The
    // fallbacks below are therefore unreachable for a created session; the
    // asserts turn a broken invariant into a loud debug failure instead of
    // putting `""` on the wire. Release builds keep the old behavior.
    let id = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    debug_assert!(!id.is_empty(), "created session row without id: {session}");
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    debug_assert!(
        !directory.is_empty(),
        "created session row without location.directory: {session}"
    );
    debug_assert!(
        session
            .get("projectID")
            .and_then(Value::as_str)
            .is_some_and(|project| !project.is_empty()),
        "created session row without projectID: {session}"
    );

    let mut map = Map::new();
    map.insert("sessionID".to_owned(), Value::String(id.clone()));
    map.insert(
        "projectID".to_owned(),
        session
            .get("projectID")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new())),
    );
    map.insert("location".to_owned(), json!({ "directory": directory }));
    for key in [
        "subpath",
        "parentID",
        "title",
        "agent",
        "model",
        "metadata",
        "permissions",
    ] {
        if let Some(value) = session.get(key) {
            map.insert(key.to_owned(), value.clone());
        }
    }
    map.insert("slug".to_owned(), Value::String(slug()));
    map.insert("version".to_owned(), Value::String(version.to_owned()));
    Value::Object(map)
}

/// Upstream-style random adjective-noun slug.
fn slug() -> String {
    use rand::Rng as _;
    const ADJECTIVES: [&str; 8] = [
        "brave", "calm", "clever", "cosmic", "gentle", "neon", "quiet", "swift",
    ];
    const NOUNS: [&str; 8] = [
        "cabin", "comet", "falcon", "harbor", "meadow", "nebula", "river", "tiger",
    ];
    let mut rng = rand::rng();
    format!(
        "{}-{}",
        ADJECTIVES[rng.random_range(0..ADJECTIVES.len())],
        NOUNS[rng.random_range(0..NOUNS.len())]
    )
}
