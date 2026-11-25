//! Compaction and inbox payloads.

use serde::{Deserialize, Serialize};

use crate::id::{MessageId, SessionId};
use crate::inbox::{Delivery, InboxItem};
use crate::message::{Native, StructuredError};
use crate::session::{ModelRef, Tokens};

use super::EventPayload;

/// Why a compaction was started or suggested.
///
/// The wire literal is the true cause; it never collapses distinct causes into
/// `auto`. `reason` stays on the events for upstream compatibility, while
/// `trigger` explains the decision to the user. [`Self::from_reason`] maps the
/// legacy reason strings a caller still holds to their trigger; `auto` yields
/// `None` because it is exactly the collapsed value this enum replaces, so the
/// caller must supply the real cause instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionTrigger {
    /// The configured context ceiling was reached.
    Guard,
    /// The provider rejected a request as too large.
    Overflow,
    /// A user or model request.
    Manual,
    /// An observer suggestion.
    Observer,
    /// A `/compress`-style command.
    Command,
}

impl CompactionTrigger {
    /// Parse a legacy compaction `reason` into its true trigger.
    ///
    /// `auto` is not a cause and yields `None`; so does any unknown string.
    #[must_use]
    pub fn from_reason(reason: &str) -> Option<Self> {
        match reason {
            "guard" => Some(Self::Guard),
            "overflow" => Some(Self::Overflow),
            "manual" => Some(Self::Manual),
            "observer" => Some(Self::Observer),
            "command" => Some(Self::Command),
            _ => None,
        }
    }
}

/// `session.compaction.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactionStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub reason: String,
    #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
    pub input_id: Option<MessageId>,
    /// Inbox row that requested/admitted this compaction, when there is one.
    #[serde(rename = "inboxID", skip_serializing_if = "Option::is_none")]
    pub inbox_id: Option<MessageId>,
    /// The true cause of this compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CompactionTrigger>,
    /// Token estimate that crossed the trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate_tokens: Option<u64>,
    /// The trigger threshold in force.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_tokens: Option<u64>,
    /// The configured hard ceiling, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling_tokens: Option<u64>,
}

impl_event_payload!(
    SessionCompactionStarted,
    "session.compaction.started",
    true,
    1
);

/// `session.compaction.ended`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactionEnded {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// Inbox row that requested/admitted this compaction, when there is one.
    #[serde(rename = "inboxID", skip_serializing_if = "Option::is_none")]
    pub inbox_id: Option<MessageId>,
    /// The true cause of this compaction (mirrors `session.compaction.started`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CompactionTrigger>,
    /// Token estimate before the compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_tokens: Option<u64>,
    /// Token estimate after the compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_tokens: Option<u64>,
}

impl_event_payload!(SessionCompactionEnded, "session.compaction.ended", true, 1);

/// `session.compaction.failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactionFailed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub reason: String,
    pub error: StructuredError,
    #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
    pub input_id: Option<MessageId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// Inbox row that requested/admitted this compaction, when there is one.
    #[serde(rename = "inboxID", skip_serializing_if = "Option::is_none")]
    pub inbox_id: Option<MessageId>,
    /// The true cause of this compaction (mirrors `session.compaction.started`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<CompactionTrigger>,
}

impl_event_payload!(
    SessionCompactionFailed,
    "session.compaction.failed",
    true,
    1
);

/// `session.compaction.suggested` — a durable advisory that a compaction is
/// worth requesting, emitted before an observer suggestion enters the inbox.
///
/// It is durable so a reloading client can still show the advisory row (and
/// its "Compact now" action) instead of only learning about it from a transient
/// toast. The row is advisory: it neither reserves a turn nor consumes tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactionSuggested {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub trigger: CompactionTrigger,
    pub estimate_tokens: u64,
    pub trigger_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ceiling_tokens: Option<u64>,
}

impl_event_payload!(
    SessionCompactionSuggested,
    "session.compaction.suggested",
    true,
    1
);

/// `session.inbox.enqueued`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInboxEnqueued {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "inboxID")]
    pub inbox_id: MessageId,
    pub item: InboxItem,
}

impl_event_payload!(SessionInboxEnqueued, "session.inbox.enqueued", true, 1);

/// `session.inbox.delivered`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInboxDelivered {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "inboxID")]
    pub inbox_id: MessageId,
}

impl_event_payload!(SessionInboxDelivered, "session.inbox.delivered", true, 1);

/// `session.inbox.cancelled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInboxCancelled {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "inboxID")]
    pub inbox_id: MessageId,
}

impl_event_payload!(SessionInboxCancelled, "session.inbox.cancelled", true, 1);

/// `session.inbox.delivery.changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInboxDeliveryChanged {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "inboxID")]
    pub inbox_id: MessageId,
    pub delivery: Delivery,
}

impl_event_payload!(
    SessionInboxDeliveryChanged,
    "session.inbox.delivery.changed",
    true,
    1
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_compaction_optionals_are_omitted_not_null() {
        let started = SessionCompactionStarted {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            reason: "auto".to_owned(),
            input_id: None,
            inbox_id: None,
            trigger: None,
            estimate_tokens: None,
            trigger_tokens: None,
            ceiling_tokens: None,
        };
        let json = serde_json::to_string(&started).unwrap();
        assert!(
            !json.contains("inboxID"),
            "absent inbox id must be omitted, got {json}"
        );
        assert!(!json.contains("trigger"), "got {json}");
        assert!(!json.contains("Tokens"), "got {json}");
        assert!(!json.contains("null"), "got {json}");

        // The compatibility field is untouched by the additions.
        let parsed: SessionCompactionStarted = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, started);

        let ended = SessionCompactionEnded {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            reason: "guard".to_owned(),
            model: None,
            native: None,
            text: "summary".to_owned(),
            cost: None,
            tokens: None,
            inbox_id: None,
            trigger: None,
            before_tokens: None,
            after_tokens: None,
        };
        let json = serde_json::to_string(&ended).unwrap();
        assert!(!json.contains("inboxID"), "got {json}");
        assert!(!json.contains("null"), "got {json}");
        assert_eq!(
            serde_json::from_str::<SessionCompactionEnded>(&json).unwrap(),
            ended
        );

        let failed = SessionCompactionFailed {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            reason: "auto".to_owned(),
            error: StructuredError::new("CompactionError", "boom"),
            input_id: None,
            cost: None,
            tokens: None,
            inbox_id: None,
            trigger: None,
        };
        let json = serde_json::to_string(&failed).unwrap();
        assert!(!json.contains("inboxID"), "got {json}");
        assert!(!json.contains("null"), "got {json}");
        assert_eq!(
            serde_json::from_str::<SessionCompactionFailed>(&json).unwrap(),
            failed
        );
    }

    #[test]
    fn compaction_trigger_uses_the_exact_wire_strings() {
        for (trigger, wire) in [
            (CompactionTrigger::Guard, "guard"),
            (CompactionTrigger::Overflow, "overflow"),
            (CompactionTrigger::Manual, "manual"),
            (CompactionTrigger::Observer, "observer"),
            (CompactionTrigger::Command, "command"),
        ] {
            assert_eq!(serde_json::to_value(trigger).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<CompactionTrigger>(serde_json::json!(wire)).unwrap(),
                trigger
            );
            assert_eq!(CompactionTrigger::from_reason(wire), Some(trigger));
        }
        assert_eq!(CompactionTrigger::from_reason("auto"), None);
        assert_eq!(CompactionTrigger::from_reason(""), None);
    }

    #[test]
    fn compaction_suggested_round_trips_with_optional_ceiling() {
        let payload = SessionCompactionSuggested {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            trigger: CompactionTrigger::Observer,
            estimate_tokens: 170_500,
            trigger_tokens: 170_000,
            ceiling_tokens: None,
        };
        let data = payload.data();
        assert_eq!(
            data,
            serde_json::json!({
                "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
                "trigger": "observer",
                "estimateTokens": 170_500,
                "triggerTokens": 170_000
            })
        );
        assert_eq!(
            <SessionCompactionSuggested as EventPayload>::TYPE,
            "session.compaction.suggested"
        );
        let round_trip: SessionCompactionSuggested = serde_json::from_value(data).unwrap();
        assert_eq!(round_trip, payload);

        let with_ceiling = SessionCompactionSuggested {
            ceiling_tokens: Some(200_000),
            ..payload
        };
        assert_eq!(with_ceiling.data()["ceilingTokens"], 200_000);
    }

    #[test]
    fn compaction_events_round_trip_with_every_trigger() {
        let started = SessionCompactionStarted {
            session_id: SessionId::from_wire("ses_0123456789abABCDEFGHIJKLMN"),
            reason: "guard".to_owned(),
            input_id: Some(MessageId::from_wire("msg_0123456789abABCDEFGHIJKLMN")),
            inbox_id: Some(MessageId::from_wire("msg_0123456789abABCDEFGHIJKLMN")),
            trigger: Some(CompactionTrigger::Guard),
            estimate_tokens: Some(175_000),
            trigger_tokens: Some(170_000),
            ceiling_tokens: Some(200_000),
        };
        let data = started.data();
        assert_eq!(data["inboxID"], "msg_0123456789abABCDEFGHIJKLMN");
        assert_eq!(data["trigger"], "guard");
        assert_eq!(data["estimateTokens"], 175_000);
        assert_eq!(data["triggerTokens"], 170_000);
        assert_eq!(data["ceilingTokens"], 200_000);
        assert_eq!(
            serde_json::from_value::<SessionCompactionStarted>(data).unwrap(),
            started
        );
    }
}
