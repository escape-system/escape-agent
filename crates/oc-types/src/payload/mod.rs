//! Typed `data` payloads for the phase-1 durable/ephemeral session events.
//!
//! The [`EventEnvelope`] keeps `data` as [`serde_json::Value`] on the
//! wire; each payload below serializes to exactly that value and can build an
//! envelope through [`EventPayload::into_envelope`].

use serde::Serialize;
use serde_json::Value;

use crate::event::EventEnvelope;
use crate::location::EventLocation;

/// Static metadata shared by every typed event payload.
pub trait EventPayload: Serialize {
    /// Exact `type` string of the event envelope.
    const TYPE: &'static str;
    /// Whether the event is persisted in the durable session log.
    const DURABLE: bool;
    /// Durable schema version; 1 unless upstream bumped it.
    const VERSION: u32;

    /// The wire `data` object.
    fn data(&self) -> Value {
        serde_json::to_value(self).expect("event payload is JSON-serializable")
    }

    /// Build a non-durable envelope carrying this payload.
    #[must_use]
    fn into_envelope(self, location: Option<EventLocation>) -> EventEnvelope
    where
        Self: Sized,
    {
        let data = serde_json::to_value(&self).expect("event payload is JSON-serializable");
        let mut envelope = EventEnvelope::now(Self::TYPE, data);
        envelope.location = location;
        envelope
    }
}

macro_rules! impl_event_payload {
    ($name:ty, $wire:literal, $durable:literal, $version:literal) => {
        impl EventPayload for $name {
            const TYPE: &'static str = $wire;
            const DURABLE: bool = $durable;
            const VERSION: u32 = $version;
        }
    };
}

/// `idle_reason` prefix used when a turn stopped because its step budget ran out.
pub const STEP_LIMIT_PREFIX: &str = "step limit reached after ";

/// Render the canonical step-limit reason.
#[must_use]
pub fn step_limit_reason(steps: u64) -> String {
    format!("{STEP_LIMIT_PREFIX}{steps} steps")
}

/// Parse a step count back out of an `idle_reason` produced by
/// [`step_limit_reason`].
///
/// Strict: the whole string must be that exact shape — the prefix, a plain
/// decimal `u64` and the ` steps` suffix. Anything else (a wrong prefix, a
/// trailing sentence, a non-digit count or an out-of-range number) is not a
/// step-limit reason and yields `None`.
#[must_use]
pub fn parse_step_limit(reason: &str) -> Option<u64> {
    let digits = reason
        .strip_prefix(STEP_LIMIT_PREFIX)?
        .strip_suffix(" steps")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

mod compaction;
mod lifecycle;
mod message;
mod meta;
mod step;
mod trace;

pub use compaction::{
    SessionCompactionEnded, SessionCompactionFailed, SessionCompactionStarted,
    SessionInboxCancelled, SessionInboxDelivered, SessionInboxDeliveryChanged,
    SessionInboxEnqueued,
};
pub use lifecycle::{
    InterruptReason, SessionArchived, SessionCreated, SessionDeleted, SessionExecutionFailed,
    SessionExecutionInterrupted, SessionExecutionStarted, SessionExecutionStepLimit,
    SessionExecutionSucceeded, SessionMoved, SessionRenamed, SessionUnarchived, SessionViewed,
};
pub use message::{
    SessionReasoningDelta, SessionReasoningEnded, SessionReasoningStarted, SessionTextDelta,
    SessionTextEnded, SessionTextStarted,
};
pub use meta::{
    SessionAgentSelected, SessionMetadataUpdated, SessionModelFallback, SessionModelSelected,
    SessionPermissions, SessionSynthetic,
};
pub use step::{
    SessionIdle, SessionStatusUpdated, SessionStepEnded, SessionStepFailed, SessionStepStarted,
    SessionStepStreamed, SessionUsageUpdated,
};
pub use trace::{
    SessionToolCalled, SessionToolFailed, SessionToolInputDelta, SessionToolInputEnded,
    SessionToolInputStarted, SessionToolProgress, SessionToolSuccess,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{MessageId, ProjectId, SessionId};
    use crate::message::ToolContent;
    use crate::session::SessionLocation;

    #[test]
    fn session_created_envelope_matches_wire_shape() {
        let payload = SessionCreated {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            project_id: ProjectId::from_wire("prj_0123456789abABCDEFGHIJKLMN"),
            location: SessionLocation::new("/work"),
            subpath: None,
            parent_id: None,
            slug: "swift-river".to_owned(),
            title: None,
            agent: None,
            model: None,
            metadata: None,
            permissions: None,
            version: "2.2.0".to_owned(),
        };
        let envelope = payload.into_envelope(Some(EventLocation {
            directory: "/work".to_owned(),
        }));
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["type"], "session.created");
        assert_eq!(
            json["data"],
            serde_json::json!({
                "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
                "projectID": "prj_0123456789abABCDEFGHIJKLMN",
                "location": {"directory": "/work"},
                "slug": "swift-river",
                "version": "2.2.0"
            })
        );
        assert_eq!(json["location"], serde_json::json!({"directory": "/work"}));
        assert!(
            !serde_json::to_string(&envelope).unwrap().contains("null"),
            "optional payload fields must be omitted"
        );
        assert_eq!(<SessionCreated as EventPayload>::TYPE, "session.created");
        const { assert!(<SessionCreated as EventPayload>::DURABLE) };
        assert_eq!(<SessionCreated as EventPayload>::VERSION, 1);
    }

    #[test]
    fn tool_success_payload_omits_optionals() {
        let payload = SessionToolSuccess {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            assistant_message_id: MessageId::from_wire("msg_0123456789abABCDEFGHIJKLMN"),
            id: "call_1".to_owned(),
            content: vec![ToolContent::Text {
                text: "ok".to_owned(),
            }],
            metadata: None,
            executed: true,
            native: None,
        };
        let data = payload.data();
        assert_eq!(
            data,
            serde_json::json!({
                "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
                "assistantMessageID": "msg_0123456789abABCDEFGHIJKLMN",
                "id": "call_1",
                "content": [{"type": "text", "text": "ok"}],
                "executed": true
            })
        );
        assert_eq!(<SessionToolSuccess as EventPayload>::VERSION, 2);
        assert!(!data.to_string().contains("null"));
    }

    #[test]
    fn every_payload_reports_an_exact_type() {
        macro_rules! check {
            ($ty:ty, $wire:literal, $durable:literal) => {{
                assert_eq!(<$ty as EventPayload>::TYPE, $wire);
                assert_eq!(<$ty as EventPayload>::DURABLE, $durable);
                assert!($wire.starts_with("session."));
            }};
        }
        check!(SessionCreated, "session.created", true);
        check!(SessionRenamed, "session.renamed", true);
        check!(SessionDeleted, "session.deleted", true);
        check!(SessionExecutionStarted, "session.execution.started", true);
        check!(
            SessionExecutionStepLimit,
            "session.execution.step_limit",
            true
        );
        check!(
            SessionExecutionSucceeded,
            "session.execution.succeeded",
            true
        );
        check!(SessionExecutionFailed, "session.execution.failed", true);
        check!(
            SessionExecutionInterrupted,
            "session.execution.interrupted",
            true
        );
        check!(SessionCompactionStarted, "session.compaction.started", true);
        check!(SessionCompactionEnded, "session.compaction.ended", true);
        check!(SessionCompactionFailed, "session.compaction.failed", true);
        check!(SessionInboxEnqueued, "session.inbox.enqueued", true);
        check!(SessionInboxDelivered, "session.inbox.delivered", true);
        check!(SessionInboxCancelled, "session.inbox.cancelled", true);
        check!(
            SessionInboxDeliveryChanged,
            "session.inbox.delivery.changed",
            true
        );
        check!(SessionAgentSelected, "session.agent.selected", true);
        check!(SessionModelSelected, "session.model.selected", true);
        check!(SessionModelFallback, "session.model.fallback", true);
        check!(SessionMetadataUpdated, "session.metadata.updated", true);
        check!(SessionPermissions, "session.permissions", true);
        check!(SessionArchived, "session.archived", true);
        check!(SessionUnarchived, "session.unarchived", true);
        check!(SessionMoved, "session.moved", true);
        check!(SessionViewed, "session.viewed", true);
        check!(SessionSynthetic, "session.synthetic", true);
        check!(SessionTextStarted, "session.text.started", true);
        check!(SessionTextDelta, "session.text.delta", false);
        check!(SessionTextEnded, "session.text.ended", true);
        check!(SessionReasoningStarted, "session.reasoning.started", true);
        check!(SessionReasoningDelta, "session.reasoning.delta", false);
        check!(SessionReasoningEnded, "session.reasoning.ended", true);
        check!(SessionToolInputStarted, "session.tool.input.started", true);
        check!(SessionToolInputDelta, "session.tool.input.delta", false);
        check!(SessionToolInputEnded, "session.tool.input.ended", true);
        check!(SessionToolCalled, "session.tool.called", true);
        check!(SessionToolProgress, "session.tool.progress", false);
        check!(SessionToolSuccess, "session.tool.success", true);
        check!(SessionToolFailed, "session.tool.failed", true);
        check!(SessionStepStarted, "session.step.started", true);
        check!(SessionStepStreamed, "session.step.streamed", true);
        check!(SessionStepEnded, "session.step.ended", true);
        check!(SessionStepFailed, "session.step.failed", true);
        check!(SessionStatusUpdated, "session.status", false);
        check!(SessionIdle, "session.idle", false);
        check!(SessionUsageUpdated, "session.usage.updated", false);
    }

    #[test]
    fn model_fallback_payload_matches_the_wire_shape() {
        use crate::session::ModelRef;

        let payload = SessionModelFallback {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            from: ModelRef::new("google", "gemini-3-pro"),
            to: ModelRef::new("opencode-go", "glm-5.3-flash"),
            kind: "empty_response".to_owned(),
            attempt: None,
        };
        let data = payload.data();
        assert_eq!(
            data,
            serde_json::json!({
                "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
                "from": {"providerID": "google", "id": "gemini-3-pro"},
                "to": {"providerID": "opencode-go", "id": "glm-5.3-flash"},
                "kind": "empty_response"
            }),
            "an unknown attempt is omitted, not null"
        );
        assert!(!data.to_string().contains("null"));
        assert_eq!(
            <SessionModelFallback as EventPayload>::TYPE,
            "session.model.fallback"
        );
        assert_eq!(<SessionModelFallback as EventPayload>::VERSION, 1);
        const { assert!(<SessionModelFallback as EventPayload>::DURABLE) };

        let with_attempt = SessionModelFallback {
            attempt: Some(2),
            ..payload
        };
        assert_eq!(with_attempt.data()["attempt"], 2);
        let round_trip: SessionModelFallback =
            serde_json::from_value(with_attempt.data()).expect("decodes");
        assert_eq!(round_trip, with_attempt);
    }

    #[test]
    fn status_payload_round_trips() {
        let fixture = serde_json::json!({
            "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
            "status": {"type": "busy"}
        });
        let parsed: SessionStatusUpdated = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
    }

    #[test]
    fn step_limit_reason_round_trips_and_parses_strictly() {
        for steps in [0, 7, u64::MAX] {
            let reason = step_limit_reason(steps);
            assert_eq!(reason, format!("step limit reached after {steps} steps"));
            assert_eq!(parse_step_limit(&reason), Some(steps));
        }
        assert_eq!(
            parse_step_limit("step limit reached after 0 steps"),
            Some(0)
        );
        assert_eq!(parse_step_limit("timeout after 5 steps"), None);
        assert_eq!(
            parse_step_limit("step limit reached after 7 steps and more"),
            None
        );
        assert_eq!(
            parse_step_limit("step limit reached after seven steps"),
            None
        );
        assert_eq!(parse_step_limit("step limit reached after +7 steps"), None);
        assert_eq!(
            parse_step_limit("step limit reached after 18446744073709551616 steps"),
            None,
            "an out-of-range count is not a valid reason"
        );
        assert_eq!(parse_step_limit(""), None);
    }

    #[test]
    fn step_limit_payload_matches_the_wire_shape() {
        let payload = SessionExecutionStepLimit {
            session_id: "ses_x".to_owned(),
            steps: 7,
        };
        let data = serde_json::to_value(&payload).unwrap();
        assert_eq!(data, serde_json::json!({"sessionID": "ses_x", "steps": 7}));
        assert!(
            !data.to_string().contains("null"),
            "the payload has no optional fields to leak as null"
        );
        let parsed: SessionExecutionStepLimit =
            serde_json::from_value(serde_json::json!({"sessionID": "ses_x", "steps": 7})).unwrap();
        assert_eq!(parsed, payload);
        assert_eq!(
            <SessionExecutionStepLimit as EventPayload>::TYPE,
            "session.execution.step_limit"
        );
        assert_eq!(<SessionExecutionStepLimit as EventPayload>::VERSION, 1);
        assert_eq!(
            <SessionExecutionStepLimit as EventPayload>::DURABLE,
            <SessionExecutionSucceeded as EventPayload>::DURABLE,
            "the qualifier is persisted wherever the terminal it qualifies is"
        );
    }
}
