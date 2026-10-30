//! Durable event persistence and publication.
//!
//! Routes persist before they publish: [`publish_durable`] allocates the event
//! id, appends the row (which also allocates the dense per-session sequence),
//! then hands the matching [`EventEnvelope`] to the SSE bus.

use oc_store::StoredEvent;
use oc_types::{Durable, EventEnvelope, EventId, EventLocation, now_millis};
use serde_json::Value;

use crate::AppState;
use crate::data::{require_store, session_not_found};
use crate::error::ErrorResponse;

/// Persist one durable event and publish it to live subscribers.
///
/// The event location is the session's stored directory, never the request
/// header. Returns the allocated per-aggregate sequence.
pub async fn publish_durable(
    state: &AppState,
    aggregate: &str,
    kind: &str,
    data: Value,
    version: u32,
) -> Result<i64, ErrorResponse> {
    let store = require_store(state)?;
    let session = store.get_session(aggregate).await?;
    let Some(session) = session else {
        return Err(session_not_found(aggregate).into());
    };
    let directory = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map_or_else(
            || state.directory().to_string_lossy().into_owned(),
            str::to_owned,
        );

    let created = now_millis();
    let id = EventId::new();
    let seq = store
        .append_event(oc_store::NewEvent {
            id: id.as_str().to_owned(),
            aggregate_id: aggregate.to_owned(),
            kind: kind.to_owned(),
            data: data.clone(),
            durable: true,
            version,
            created,
        })
        .await?;

    let envelope = EventEnvelope {
        id,
        created: Some(created),
        kind: kind.to_owned(),
        metadata: None,
        location: Some(EventLocation { directory }),
        data,
        durable: Some(Durable {
            aggregate_id: aggregate.to_owned(),
            seq,
            version,
        }),
    };
    state.publish(envelope);
    Ok(seq)
}

/// Rebuild the wire envelope for a persisted event.
#[must_use]
pub fn from_stored(event: StoredEvent, directory: &str) -> EventEnvelope {
    EventEnvelope {
        id: EventId::from_wire(event.id),
        created: Some(event.created),
        kind: event.kind,
        metadata: None,
        location: Some(EventLocation {
            directory: directory.to_owned(),
        }),
        data: event.data,
        durable: Some(Durable {
            aggregate_id: event.aggregate_id,
            seq: event.seq,
            version: event.version,
        }),
    }
}
