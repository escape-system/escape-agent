//! Event envelope shared by the SSE feed and the durable session log.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::EventId;
use crate::location::EventLocation;

/// Durable marker present only on persisted events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Durable {
    #[serde(rename = "aggregateID")]
    pub aggregate_id: String,
    pub seq: i64,
    pub version: u32,
}

/// Wire envelope for `GET /api/event` and `GET /api/session/:id/log`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: EventId,
    /// Omitted on `server.connected` (upstream behavior).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<EventLocation>,
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durable: Option<Durable>,
}

impl EventEnvelope {
    /// Build a non-durable event with the current timestamp.
    #[must_use]
    pub fn now(kind: impl Into<String>, data: Value) -> Self {
        Self {
            id: EventId::new(),
            created: Some(crate::now_millis()),
            kind: kind.into(),
            metadata: None,
            location: None,
            data,
            durable: None,
        }
    }

    /// The mandatory first frame of every SSE connection.
    #[must_use]
    pub fn connected() -> Self {
        Self {
            id: EventId::new(),
            created: None,
            kind: "server.connected".to_owned(),
            metadata: None,
            location: None,
            data: Value::Object(serde_json::Map::new()),
            durable: None,
        }
    }

    /// Serialize the envelope for an SSE `data:` line.
    ///
    /// # Panics
    ///
    /// Panics only if the envelope contains a non-string map key, which cannot
    /// happen for values built by this crate.
    #[must_use]
    pub fn to_sse_data(&self) -> String {
        serde_json::to_string(self).expect("event envelope is serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connected_has_no_created_or_durable() {
        let json = serde_json::to_value(EventEnvelope::connected()).unwrap();
        assert_eq!(json["type"], "server.connected");
        assert!(json.get("created").is_none());
        assert!(json.get("durable").is_none());
        assert_eq!(json["data"], serde_json::json!({}));
    }

    #[test]
    fn optional_fields_are_omitted_not_null() {
        let event = EventEnvelope::now("test.event", serde_json::json!({"a": 1}));
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("null"));
        assert!(json.contains("\"type\":\"test.event\""));
    }
}
