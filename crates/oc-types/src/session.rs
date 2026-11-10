//! Session entities: `Session.Info`, list filters, pagination and status.
//!
//! Mirrors `Session.Info`, `Session.ListFilter`, `Session.ListCursor` and the
//! cursor envelope from `packages/client/src/promise/generated/types.ts`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::file::FileDiff;
use crate::id::{MessageId, ProjectId, SessionId};
use crate::location::ProjectRef;
use crate::permission::PermissionRule;

/// Free-form session metadata (`Session.Metadata`).
pub type Metadata = Map<String, Value>;

/// `TokenUsage.Info.cache`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CacheTokens {
    pub read: i64,
    pub write: i64,
}

/// `TokenUsage.Info`.
///
/// `input` is the **non-cached** prompt part. The provider prompt size is
/// [`Tokens::prompt_tokens`]; this disjoint shape matches the vendored
/// `token-usage` contract, whose `total()` adds every field.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache: CacheTokens,
}

impl Tokens {
    /// Inclusive prompt size: non-cached input plus cache reads/writes.
    ///
    /// Use this for context-window and compaction math; `input` alone is
    /// non-cached and a fully cached step can report `input == 0`.
    #[must_use]
    pub fn prompt_tokens(&self) -> i64 {
        self.input
            .saturating_add(self.cache.read)
            .saturating_add(self.cache.write)
    }
}

/// `Model.Ref`: a model reference as carried on the wire (`providerID`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl ModelRef {
    #[must_use]
    pub fn new(provider_id: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            provider_id: provider_id.into(),
            variant: None,
        }
    }

    /// Render `provider/id` or `provider/id#variant`.
    #[must_use]
    pub fn to_reference(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}/{}#{variant}", self.provider_id, self.id),
            None => format!("{}/{}", self.provider_id, self.id),
        }
    }
}

/// `Location.Ref` as embedded in sessions and session-scoped event payloads.
///
/// Distinct from [`crate::location::LocationRef`], which carries the project
/// in the `{location, data}` response envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLocation {
    pub directory: String,
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

impl SessionLocation {
    #[must_use]
    pub fn new(directory: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            workspace_id: None,
        }
    }
}

impl From<crate::location::EventLocation> for SessionLocation {
    fn from(value: crate::location::EventLocation) -> Self {
        Self::new(value.directory)
    }
}

/// `Location.PublicInfo` plus the session `subpath`, as listed in docs/17.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationInfo {
    pub directory: String,
    pub project: ProjectRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// `Session.Fork.Boundary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ForkBoundary {
    #[serde(rename = "before")]
    Before {
        #[serde(rename = "messageID")]
        message_id: MessageId,
    },
    #[serde(rename = "through")]
    Through {
        #[serde(rename = "messageID")]
        message_id: MessageId,
    },
}

/// `Session.Info.fork`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkRef {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub boundary: ForkBoundary,
}

/// `Session.Info.time`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTimes {
    pub created: i64,
    pub updated: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub busy: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<i64>,
}

/// Outcome of the last completed execution (`Session.Info.outcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

/// `Session.Revert`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRevert {
    #[serde(rename = "messageID")]
    pub message_id: MessageId,
    #[serde(rename = "partID", skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileDiff>>,
}

/// Reads `Session.Info.outcome` tolerantly.
///
/// The wire model has exactly three literals ([`SessionOutcome`]), but the
/// runtime stores one internal literal the wire does not know:
/// `"step_limit"` (a completed turn whose step budget ended it). A strict
/// parse would make that session unreadable on its next execution, so an
/// unknown string reads as `None` — "no wire outcome" — and is omitted when
/// the value is re-serialized. Wrong JSON types still fail.
fn deserialize_outcome<'de, D>(deserializer: D) -> Result<Option<SessionOutcome>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.and_then(|raw| match raw.as_str() {
        "succeeded" => Some(SessionOutcome::Succeeded),
        "failed" => Some(SessionOutcome::Failed),
        "interrupted" => Some(SessionOutcome::Interrupted),
        _ => None,
    }))
}

/// Exact wire `Session.Info`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: SessionId,
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork: Option<ForkRef>,
    #[serde(rename = "projectID")]
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub cost: f64,
    pub tokens: Tokens,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_outcome"
    )]
    pub outcome: Option<SessionOutcome>,
    pub time: SessionTimes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub location: SessionLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    /// Evaluated after the agent's rules; the last matching rule wins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<PermissionRule>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert: Option<SessionRevert>,
}

/// `Session.Status` as broadcast by `session.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionStatus {
    Idle,
    Retry {
        attempt: u32,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<RetryAction>,
        next: i64,
    },
    Busy,
}

/// Human-readable retry remediation attached to [`SessionStatus::Retry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryAction {
    pub reason: String,
    pub provider: String,
    pub title: String,
    pub message: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// Sort order for session and message listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    Asc,
    Desc,
}

/// `GET /api/session` query parameters.
///
/// `parent_id` uses the literal string `"null"` to request root sessions, per
/// docs/02. `archived` is the literal `true`/`false` query value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionListQuery {
    pub limit: Option<u32>,
    pub order: Option<Order>,
    pub search: Option<String>,
    pub parent_id: Option<String>,
    pub archived: Option<bool>,
    pub directory: Option<String>,
    pub project: Option<ProjectId>,
    pub subpath: Option<String>,
    pub cursor: Option<String>,
}

/// Pagination cursors of a session list response. Absent anchors are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `{data, cursor}` response of `GET /api/session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub data: Vec<SessionInfo>,
    pub cursor: Cursor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_reference_round_trips() {
        let model = ModelRef::new("anthropic", "claude-sonnet-4");
        assert_eq!(model.to_reference(), "anthropic/claude-sonnet-4");
        let with_variant = ModelRef {
            variant: Some("high".to_owned()),
            ..model
        };
        assert_eq!(
            with_variant.to_reference(),
            "anthropic/claude-sonnet-4#high"
        );
    }

    #[test]
    fn session_status_discriminators_are_exact() {
        let idle = serde_json::to_value(SessionStatus::Idle).unwrap();
        assert_eq!(idle, serde_json::json!({"type": "idle"}));
        let busy = serde_json::to_value(SessionStatus::Busy).unwrap();
        assert_eq!(busy, serde_json::json!({"type": "busy"}));
        let retry = serde_json::to_value(SessionStatus::Retry {
            attempt: 2,
            message: "rate limited".to_owned(),
            action: None,
            next: 1_730_000_000_000,
        })
        .unwrap();
        assert_eq!(
            retry,
            serde_json::json!({
                "type": "retry",
                "attempt": 2,
                "message": "rate limited",
                "next": 1_730_000_000_000i64
            })
        );
        assert!(!retry.to_string().contains("null"));
    }

    #[test]
    fn session_list_response_round_trips() {
        let fixture = serde_json::json!({
            "data": [{
                "id": "ses_0123456789abABCDEFGHIJKLMN",
                "projectID": "prj_0123456789abABCDEFGHIJKLMN",
                "cost": 0.0,
                "tokens": {
                    "input": 10,
                    "output": 5,
                    "reasoning": 2,
                    "cache": {"read": 3, "write": 1}
                },
                "time": {"created": 1_730_000_000_000i64, "updated": 1_730_000_001_000i64},
                "location": {"directory": "/work"}
            }],
            "cursor": {}
        });
        let parsed: SessionListResponse = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(parsed.data[0].title, None);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            fixture,
            "session list must round-trip"
        );
        let text = serde_json::to_string(&parsed).unwrap();
        assert!(
            !text.contains("null"),
            "optional fields must be omitted: {text}"
        );
    }

    fn session_with_outcome(outcome: &Value) -> Value {
        serde_json::json!({
            "id": "ses_0123456789abABCDEFGHIJKLMN",
            "projectID": "prj_0123456789abABCDEFGHIJKLMN",
            "cost": 0.0,
            "tokens": {
                "input": 1,
                "output": 1,
                "reasoning": 0,
                "cache": {"read": 0, "write": 0}
            },
            "outcome": outcome.clone(),
            "time": {"created": 1_730_000_000_000i64, "updated": 1_730_000_000_000i64},
            "location": {"directory": "/work"}
        })
    }

    #[test]
    fn known_outcome_literals_round_trip() {
        for (literal, expected) in [
            ("succeeded", SessionOutcome::Succeeded),
            ("failed", SessionOutcome::Failed),
            ("interrupted", SessionOutcome::Interrupted),
        ] {
            let fixture = session_with_outcome(&Value::String(literal.to_owned()));
            let parsed: SessionInfo = serde_json::from_value(fixture.clone()).unwrap();
            assert_eq!(parsed.outcome, Some(expected), "{literal}");
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                fixture,
                "{literal} must round-trip unchanged"
            );
        }
    }

    #[test]
    fn an_unknown_outcome_reads_as_absent_and_is_omitted() {
        // `step_limit` is the runtime's durable literal for a completed turn
        // whose step budget ended it. The wire enum has no such variant, so a
        // tolerant read maps it to `None` instead of failing the whole session.
        let fixture = session_with_outcome(&Value::String("step_limit".to_owned()));
        let parsed: SessionInfo = serde_json::from_value(fixture).expect("unknown outcome parses");
        assert_eq!(parsed.outcome, None);

        let reserialized = serde_json::to_value(&parsed).unwrap();
        assert!(
            reserialized.get("outcome").is_none(),
            "an unreadable outcome is omitted, never null: {reserialized}"
        );
        assert!(
            !reserialized.to_string().contains("null"),
            "optional fields must be omitted: {reserialized}"
        );

        // An unrelated unknown literal behaves the same way.
        let parsed: SessionInfo =
            serde_json::from_value(session_with_outcome(&Value::String("cancelled".to_owned())))
                .expect("unrelated unknown outcome parses");
        assert_eq!(parsed.outcome, None);
    }

    #[test]
    fn an_explicitly_absent_outcome_stays_absent() {
        let fixture = serde_json::json!({
            "id": "ses_0123456789abABCDEFGHIJKLMN",
            "projectID": "prj_0123456789abABCDEFGHIJKLMN",
            "cost": 0.0,
            "tokens": {
                "input": 0,
                "output": 0,
                "reasoning": 0,
                "cache": {"read": 0, "write": 0}
            },
            "time": {"created": 1, "updated": 1},
            "location": {"directory": "/work"}
        });
        let parsed: SessionInfo = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(parsed.outcome, None);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), fixture);
    }
}
