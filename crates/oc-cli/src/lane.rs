//! Lane concurrency configuration and lane event publication.

use std::sync::Arc;

use oc_types::{EventEnvelope, EventLocation};

/// Lane concurrency from the merged `lanes` config object, or the defaults.
///
/// The documented wire shape is a flat map (`{"cargo": 1, "default": 8}`),
/// where `default` and `max_queue` are reserved keys. A serialized
/// [`oc_lanes::LaneConfig`] is accepted too; it is unambiguous because the
/// typed shape always carries a `lanes` object.
pub(crate) fn lane_config(config: &oc_config::ConfigService) -> oc_lanes::LaneConfig {
    lane_config_from_value(config.merged().get("lanes"))
}

fn lane_config_from_value(value: Option<&serde_json::Value>) -> oc_lanes::LaneConfig {
    let Some(value) = value.and_then(serde_json::Value::as_object) else {
        return oc_lanes::LaneConfig::default();
    };
    // `max_queue` also exists in the flat shape, so only the nested `lanes`
    // object can identify a serialized `LaneConfig`.
    if value.get("lanes").is_some_and(serde_json::Value::is_object) {
        return serde_json::from_value(serde_json::Value::Object(value.clone())).unwrap_or_else(
            |error| {
                tracing::warn!(%error, "invalid `lanes` configuration; using defaults");
                oc_lanes::LaneConfig::default()
            },
        );
    }
    let mut lanes = oc_lanes::LaneConfig::default();
    for (lane, value) in value {
        let Some(concurrency) = value.as_u64().and_then(|n| usize::try_from(n).ok()) else {
            tracing::warn!(%lane, "ignoring non-numeric lane concurrency");
            continue;
        };
        match lane.as_str() {
            "default" => lanes.default_concurrency = concurrency,
            "max_queue" => lanes.max_queue = concurrency,
            _ => {
                lanes.lanes.insert(lane.clone(), concurrency);
            }
        }
    }
    lanes
}

/// Publishes lane lifecycle changes as ephemeral SSE events.
#[derive(Clone)]
pub(crate) struct BusLaneEvents {
    pub(crate) bus: tokio::sync::broadcast::Sender<Arc<EventEnvelope>>,
    pub(crate) directory: String,
}

impl oc_lanes::LaneEvents for BusLaneEvents {
    fn changed(&self, ticket: &oc_lanes::Ticket) {
        let mut data = serde_json::json!({
            "lane": &ticket.lane,
            "ticket": ticket,
        });
        if let Some(session) = &ticket.owner_session {
            data["sessionID"] = serde_json::Value::String(session.clone());
        }
        let mut event = EventEnvelope::now(lane_event_kind(&ticket.state), data);
        event.location = Some(EventLocation {
            directory: self.directory.clone(),
        });
        // A send error only means no subscriber is connected.
        let _ = self.bus.send(Arc::new(event));
    }
}

fn lane_event_kind(state: &oc_lanes::TicketState) -> &'static str {
    match state {
        oc_lanes::TicketState::Queued => "lane.queued",
        oc_lanes::TicketState::Running => "lane.started",
        oc_lanes::TicketState::Succeeded { .. } | oc_lanes::TicketState::Failed { .. } => {
            "lane.finished"
        }
        oc_lanes::TicketState::Cancelled => "lane.cancelled",
    }
}

#[cfg(test)]
mod tests {
    use oc_lanes::LaneConfig;
    use serde_json::json;

    use super::lane_config_from_value;

    #[test]
    fn lane_config_defaults_when_absent() {
        assert_eq!(lane_config_from_value(None), LaneConfig::default());
        assert_eq!(
            lane_config_from_value(Some(&json!("invalid"))),
            LaneConfig::default()
        );
    }

    #[test]
    fn lane_config_reads_the_documented_flat_shape() {
        let config = lane_config_from_value(Some(&json!({
            "cargo": 2,
            "gpu": 1,
            "default": 3,
            "max_queue": 9,
        })));
        assert_eq!(config.concurrency_for("cargo:workspace"), 1);
        assert_eq!(config.lanes.get("cargo"), Some(&2));
        assert_eq!(config.lanes.get("gpu"), Some(&1));
        assert_eq!(config.default_concurrency, 3);
        assert_eq!(config.max_queue, 9);
    }

    #[test]
    fn lane_config_flat_shape_keeps_max_queue_flat() {
        let config = lane_config_from_value(Some(&json!({
            "cargo": 2,
            "network": 4,
            "max_queue": 11,
        })));
        assert_eq!(config.lanes.get("cargo"), Some(&2));
        assert_eq!(config.lanes.get("network"), Some(&4));
        assert_eq!(config.max_queue, 11);
        assert_eq!(
            config.default_concurrency,
            LaneConfig::default().default_concurrency
        );
    }

    #[test]
    fn lane_config_typed_empty_lanes_is_not_flat() {
        let config = lane_config_from_value(Some(&json!({
            "lanes": {},
            "default_concurrency": 6,
            "max_queue": 12,
        })));
        assert!(config.lanes.is_empty());
        assert_eq!(config.default_concurrency, 6);
        assert_eq!(config.max_queue, 12);
    }

    #[test]
    fn lane_config_reads_the_typed_shape() {
        let config = lane_config_from_value(Some(&json!({
            "lanes": { "gpu": 2 },
            "default_concurrency": 5,
            "max_queue": 7,
        })));
        assert_eq!(config.lanes.get("gpu"), Some(&2));
        assert_eq!(config.default_concurrency, 5);
        assert_eq!(config.max_queue, 7);
    }
}
