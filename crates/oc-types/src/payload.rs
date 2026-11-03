//! Typed `data` payloads for the phase-1 durable/ephemeral session events.
//!
//! The [`EventEnvelope`] keeps `data` as [`serde_json::Value`] on the
//! wire; each payload below serializes to exactly that value and can build an
//! envelope through [`EventPayload::into_envelope`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::EventEnvelope;
use crate::id::{MessageId, ProjectId, SessionId};
use crate::inbox::{Delivery, InboxItem};
use crate::location::EventLocation;
use crate::message::{FinishReason, Native, StructuredError, ToolContent};
use crate::permission::PermissionRule;
use crate::session::{ModelRef, SessionLocation, SessionStatus, Tokens};

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

/// `session.created`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreated {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    pub location: SessionLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Native>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<PermissionRule>>,
    pub version: String,
}

impl_event_payload!(SessionCreated, "session.created", true, 1);

/// `session.renamed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRenamed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub title: String,
}

impl_event_payload!(SessionRenamed, "session.renamed", true, 1);

/// `session.deleted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeleted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

impl_event_payload!(SessionDeleted, "session.deleted", true, 2);

/// `session.execution.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExecutionStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

impl_event_payload!(
    SessionExecutionStarted,
    "session.execution.started",
    true,
    1
);

/// `session.execution.succeeded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExecutionSucceeded {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

impl_event_payload!(
    SessionExecutionSucceeded,
    "session.execution.succeeded",
    true,
    1
);

/// `session.execution.failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExecutionFailed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub error: StructuredError,
}

impl_event_payload!(SessionExecutionFailed, "session.execution.failed", true, 1);

/// `Session.Execution.InterruptReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterruptReason {
    User,
    Shutdown,
    Superseded,
    Inactivity,
}

/// `session.execution.interrupted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionExecutionInterrupted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub reason: InterruptReason,
}

impl_event_payload!(
    SessionExecutionInterrupted,
    "session.execution.interrupted",
    true,
    1
);

/// `session.compaction.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactionStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub reason: String,
    #[serde(rename = "inputID", skip_serializing_if = "Option::is_none")]
    pub input_id: Option<MessageId>,
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
}

impl_event_payload!(
    SessionCompactionFailed,
    "session.compaction.failed",
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

/// `session.agent.selected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAgentSelected {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

impl_event_payload!(SessionAgentSelected, "session.agent.selected", true, 1);

/// `session.model.selected`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelSelected {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub model: ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<ModelRef>,
}

impl_event_payload!(SessionModelSelected, "session.model.selected", true, 1);

/// `session.metadata.updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadataUpdated {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub metadata: Native,
}

impl_event_payload!(SessionMetadataUpdated, "session.metadata.updated", true, 1);

/// `session.permissions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPermissions {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub permissions: Vec<PermissionRule>,
}

impl_event_payload!(SessionPermissions, "session.permissions", true, 1);

/// `session.archived`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionArchived {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

impl_event_payload!(SessionArchived, "session.archived", true, 1);

/// `session.unarchived`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnarchived {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

impl_event_payload!(SessionUnarchived, "session.unarchived", true, 1);

/// `session.moved`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMoved {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub location: SessionLocation,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

impl_event_payload!(SessionMoved, "session.moved", true, 1);

/// `session.viewed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionViewed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub idle: i64,
}

impl_event_payload!(SessionViewed, "session.viewed", true, 1);

/// `session.synthetic`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSynthetic {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Native>,
}

impl_event_payload!(SessionSynthetic, "session.synthetic", true, 1);

/// `session.text.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTextStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub ordinal: u32,
}

impl_event_payload!(SessionTextStarted, "session.text.started", true, 1);

/// `session.text.delta` (ephemeral).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTextDelta {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub ordinal: u32,
    pub delta: String,
}

impl_event_payload!(SessionTextDelta, "session.text.delta", false, 1);

/// `session.text.ended`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTextEnded {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub ordinal: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

impl_event_payload!(SessionTextEnded, "session.text.ended", true, 1);

/// `session.reasoning.started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReasoningStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub ordinal: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

impl_event_payload!(
    SessionReasoningStarted,
    "session.reasoning.started",
    true,
    1
);

/// `session.reasoning.delta` (ephemeral).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReasoningDelta {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub ordinal: u32,
    pub delta: String,
}

impl_event_payload!(SessionReasoningDelta, "session.reasoning.delta", false, 1);

/// `session.reasoning.ended`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReasoningEnded {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub ordinal: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

impl_event_payload!(SessionReasoningEnded, "session.reasoning.ended", true, 1);

/// `session.tool.input.started`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolInputStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub name: String,
}

impl_event_payload!(
    SessionToolInputStarted,
    "session.tool.input.started",
    true,
    1
);

/// `session.tool.input.delta` (ephemeral).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolInputDelta {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub delta: String,
}

impl_event_payload!(SessionToolInputDelta, "session.tool.input.delta", false, 1);

/// `session.tool.input.ended`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolInputEnded {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub text: String,
}

impl_event_payload!(SessionToolInputEnded, "session.tool.input.ended", true, 1);

/// `session.tool.called`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolCalled {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub input: serde_json::Map<String, Value>,
    pub executed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

impl_event_payload!(SessionToolCalled, "session.tool.called", true, 1);

/// `session.tool.progress` (ephemeral).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolProgress {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub metadata: Native,
}

impl_event_payload!(SessionToolProgress, "session.tool.progress", false, 1);

/// `session.tool.success`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolSuccess {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub content: Vec<ToolContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Native>,
    pub executed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

impl_event_payload!(SessionToolSuccess, "session.tool.success", true, 2);

/// `session.tool.failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolFailed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub id: String,
    pub error: StructuredError,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolContent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Native>,
    pub executed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

impl_event_payload!(SessionToolFailed, "session.tool.failed", true, 2);

/// `session.step.started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStepStarted {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub agent: String,
    pub model: ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub started: i64,
}

impl_event_payload!(SessionStepStarted, "session.step.started", true, 1);

/// `session.step.streamed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStepStreamed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
}

impl_event_payload!(SessionStepStreamed, "session.step.streamed", true, 1);

/// `session.step.ended`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStepEnded {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub finish: FinishReason,
    #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
    pub raw_finish: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    pub cost: f64,
    pub tokens: Tokens,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

impl_event_payload!(SessionStepEnded, "session.step.ended", true, 1);

/// `session.step.failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStepFailed {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    #[serde(rename = "assistantMessageID")]
    pub assistant_message_id: MessageId,
    pub error: StructuredError,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishReason>,
    #[serde(rename = "rawFinish", skip_serializing_if = "Option::is_none")]
    pub raw_finish: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

impl_event_payload!(SessionStepFailed, "session.step.failed", true, 1);

/// `session.status` (ephemeral).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusUpdated {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub status: SessionStatus,
}

impl_event_payload!(SessionStatusUpdated, "session.status", false, 1);

/// `session.idle` (ephemeral).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdle {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

impl_event_payload!(SessionIdle, "session.idle", false, 1);

/// `session.usage.updated` (ephemeral).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageUpdated {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub cost: f64,
    pub tokens: Tokens,
}

impl_event_payload!(SessionUsageUpdated, "session.usage.updated", false, 1);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SessionId;

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
    fn status_payload_round_trips() {
        let fixture = serde_json::json!({
            "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
            "status": {"type": "busy"}
        });
        let parsed: SessionStatusUpdated = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
    }
}
