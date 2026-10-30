//! Permission requests, saved permissions and rule effects.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::id::{PermissionRequestId, ProjectId, SavedPermissionId, SessionId};

/// `Permission.Effect`, also used by config rules and `Agent.Info.permissions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

/// `Permission.Rule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

impl PermissionRule {
    #[must_use]
    pub fn new(action: impl Into<String>, resource: impl Into<String>, effect: Effect) -> Self {
        Self {
            action: action.into(),
            resource: resource.into(),
            effect,
        }
    }
}

/// `Permission.Source.type`. Only tool usage is defined upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionSourceKind {
    Tool,
}

/// `Permission.Source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSource {
    #[serde(rename = "type")]
    pub kind: PermissionSourceKind,
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub id: String,
}

/// `Permission.Request` as returned by the permission routes and `permission.asked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: PermissionRequestId,
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub action: String,
    pub resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PermissionSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// `Permission.SavedInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSaved {
    pub id: SavedPermissionId,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    pub action: String,
    pub resource: String,
    pub time: PermissionSavedTime,
}

/// `Permission.SavedInfo.time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSavedTime {
    pub created: i64,
    pub updated: i64,
}

/// `Permission.Reply` for `POST .../permission/:requestID/reply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_request_round_trips_without_nulls() {
        let fixture = serde_json::json!({
            "id": "per_0123456789abABCDEFGHIJKLMN",
            "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
            "action": "read",
            "resources": ["/work/a.env"],
            "save": ["read"],
            "metadata": {"tool": "read"},
            "source": {
                "type": "tool",
                "messageID": "msg_0123456789abABCDEFGHIJKLMN",
                "id": "call_1"
            },
            "message": "Allow reading secrets?"
        });
        let parsed: PermissionRequest = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
        assert!(!serde_json::to_string(&parsed).unwrap().contains("null"));
    }

    #[test]
    fn minimal_permission_request_omits_optional_fields() {
        let fixture = serde_json::json!({
            "id": "per_0123456789abABCDEFGHIJKLMN",
            "sessionID": "ses_0123456789abABCDEFGHIJKLMN",
            "action": "read",
            "resources": ["*"]
        });
        let parsed: PermissionRequest = serde_json::from_value(fixture.clone()).unwrap();
        let out = serde_json::to_value(&parsed).unwrap();
        assert_eq!(out, fixture);
        assert!(out.get("save").is_none());
        assert!(out.get("message").is_none());
        assert!(out.get("source").is_none());
    }

    #[test]
    fn effect_discriminators_are_lowercase() {
        assert_eq!(serde_json::to_value(Effect::Allow).unwrap(), "allow");
        assert_eq!(serde_json::to_value(Effect::Deny).unwrap(), "deny");
        assert_eq!(serde_json::to_value(Effect::Ask).unwrap(), "ask");
        assert_eq!(serde_json::to_value(PermissionReply::Once).unwrap(), "once");
        assert_eq!(
            serde_json::to_value(PermissionReply::Always).unwrap(),
            "always"
        );
        assert_eq!(
            serde_json::to_value(PermissionReply::Reject).unwrap(),
            "reject"
        );
    }
}
