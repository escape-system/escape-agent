//! Session selection, metadata, permission and synthetic-message payloads.

use serde::{Deserialize, Serialize};

use crate::id::SessionId;
use crate::message::Native;
use crate::permission::PermissionRule;
use crate::session::ModelRef;

use super::EventPayload;

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

/// `session.model.fallback`.
///
/// `kind` is the failure class as a `snake_case` string (`empty_response`,
/// `transport`, `rate_limit`, `quota`); `attempt` is the 1-based fallback hop
/// and is omitted when unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelFallback {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub from: ModelRef,
    pub to: ModelRef,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

impl_event_payload!(SessionModelFallback, "session.model.fallback", true, 1);

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
