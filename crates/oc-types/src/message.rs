//! `Session.Message.Info`: the transcript message tagged union.
//!
//! Assistant content parts, the tool state machine and all message variants
//! mirror `packages/schema/src/session-message.ts` exactly.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::id::{MessageId, ProjectId};
use crate::session::{Metadata, ModelRef, SessionLocation, SessionOutcome, Tokens};

/// Provider-owned replay data (`Session.Message.Native`).
pub type Native = Map<String, Value>;

/// `Session.StructuredError`, the error shape used in messages, tool states
/// and execution events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ErrorResponse>,
    #[serde(rename = "retryAfterMs", skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,
}

/// `Session.StructuredError.response`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub body: String,
}

impl StructuredError {
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            status: None,
            response: None,
            retry_after_ms: None,
        }
    }
}

/// `Session.Message.Info.time` for messages with only a creation stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageTime {
    pub created: i64,
}

/// Creation/completion stamp shared by shells and reasoning parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletedTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// Assistant message timing: request start, response stream end, settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streamed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// Tool part timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTime {
    pub created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// A transcript message, flattened with its `type` discriminator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(flatten)]
    pub body: MessageBody,
}

impl Message {
    /// The exact wire discriminator of this message.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        self.body.kind()
    }
}

impl From<MessageBody> for Message {
    fn from(body: MessageBody) -> Self {
        Self { body }
    }
}

/// `Session.Message.Info`, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageBody {
    #[serde(rename = "agent-switched")]
    AgentSelected(AgentSwitchedMessage),
    #[serde(rename = "model-switched")]
    ModelSelected(ModelSwitchedMessage),
    #[serde(rename = "location-switched")]
    LocationSelected(LocationSwitchedMessage),
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "synthetic")]
    Synthetic(SyntheticMessage),
    #[serde(rename = "system")]
    System(SystemMessage),
    #[serde(rename = "skill")]
    Skill(SkillMessage),
    #[serde(rename = "shell")]
    Shell(ShellMessage),
    #[serde(rename = "assistant")]
    Assistant(Box<AssistantMessage>),
    #[serde(rename = "compaction")]
    Compaction(CompactionMessage),
    #[serde(rename = "idle")]
    Idle(IdleMessage),
}

impl MessageBody {
    /// The exact wire discriminator of this message body.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AgentSelected(_) => "agent-switched",
            Self::ModelSelected(_) => "model-switched",
            Self::LocationSelected(_) => "location-switched",
            Self::User(_) => "user",
            Self::Synthetic(_) => "synthetic",
            Self::System(_) => "system",
            Self::Skill(_) => "skill",
            Self::Shell(_) => "shell",
            Self::Assistant(_) => "assistant",
            Self::Compaction(_) => "compaction",
            Self::Idle(_) => "idle",
        }
    }
}

/// `Session.Message.AgentSelected`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSwitchedMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

/// `Session.Message.ModelSelected`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchedMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub model: ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<ModelRef>,
}

/// `Session.Message.LocationSwitched.previous`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviousLocation {
    pub location: SessionLocation,
    #[serde(rename = "projectID", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// `Session.Message.LocationSwitched`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocationSwitchedMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub location: SessionLocation,
    #[serde(rename = "projectID", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<PreviousLocation>,
}

/// `Prompt.FileAttachment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFileAttachment {
    pub data: String,
    pub mime: String,
    pub source: PromptFileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

/// `Prompt.FileSource`, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PromptFileSource {
    #[serde(rename = "inline")]
    Inline {},
    #[serde(rename = "uri")]
    Uri { uri: String },
}

/// `Prompt.Mention`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptMention {
    pub start: i64,
    pub end: i64,
    pub text: String,
}

/// `Prompt.AgentAttachment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAgentAttachment {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

/// `Prompt.SkillAttachment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptSkillAttachment {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention: Option<PromptMention>,
}

/// `Session.Message.User`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<PromptFileAttachment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<PromptAgentAttachment>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<PromptSkillAttachment>>,
}

/// `Session.Message.Synthetic`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyntheticMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `Session.Message.System`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// `Session.Message.Skill`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub skill: String,
    pub name: String,
    pub text: String,
}

/// `Shell.Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellStatus {
    Running,
    Exited,
    Timeout,
    Killed,
}

/// A shell exit value: a number, or one of the JS `Infinity`/`NaN` strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ShellExit {
    Code(i64),
    Special(String),
}

/// `Shell.Output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellOutput {
    pub output: String,
    pub cursor: i64,
    pub size: i64,
    pub truncated: bool,
}

/// `Session.Message.Shell`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: CompletedTime,
    #[serde(rename = "shellID")]
    pub shell_id: String,
    pub command: String,
    pub status: ShellStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<ShellExit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<ShellOutput>,
}

/// `Tool.Content`, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "file")]
    File {
        uri: String,
        mime: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// `Session.Message.ToolState`, tagged by `status`.
///
/// `content` on `completed`/`error` is non-empty on the wire
/// (`Schema.NonEmptyArray(Content)`); deserialization does not reject an empty
/// array, while our own emitters always provide at least one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum ToolState {
    Streaming {
        input: String,
    },
    Running {
        input: Map<String, Value>,
        /// Required upstream; tolerated as absent here.
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
    Completed {
        input: Map<String, Value>,
        content: Vec<ToolContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
    Error {
        input: Map<String, Value>,
        error: StructuredError,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ToolContent>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<Map<String, Value>>,
    },
}

/// `Session.Message.Assistant.Text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPart {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
}

/// `Session.Message.Assistant.Reasoning`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningPart {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<CompletedTime>,
}

/// `Session.Message.Assistant.Tool`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPart {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    pub state: ToolState,
    pub time: ToolTime,
}

/// `Session.Message.AssistantContent`, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantPart {
    #[serde(rename = "text")]
    Text(TextPart),
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningPart),
    #[serde(rename = "tool")]
    Tool(Box<ToolPart>),
}

/// `FinishReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}

/// `Session.Message.Assistant.snapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
}

/// `Session.Message.Assistant.Retry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantRetry {
    pub attempt: u32,
    pub at: i64,
    pub error: StructuredError,
}

/// `Session.Message.Assistant`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: AssistantTime,
    pub agent: String,
    pub model: ModelRef,
    pub content: Vec<AssistantPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
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
    pub error: Option<StructuredError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<AssistantRetry>,
}

/// `Compaction.Reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionReason {
    Auto,
    Manual,
}

/// `Session.Message.Compaction.Running`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRunning {
    pub reason: CompactionReason,
    pub summary: String,
}

/// `Session.Message.Compaction.Completed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCompleted {
    pub reason: CompactionReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// `Session.Message.Compaction.Failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailed {
    pub reason: CompactionReason,
    pub error: StructuredError,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// `Session.Message.Compaction` status union, tagged by `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum CompactionStatus {
    Running(CompactionRunning),
    Completed(CompactionCompleted),
    Failed(CompactionFailed),
}

/// `Session.Message.Compaction`: the shared message base plus its status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    #[serde(flatten)]
    pub status: CompactionStatus,
}

/// `Session.Message.Idle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleMessage {
    pub id: MessageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    pub time: MessageTime,
    pub outcome: SessionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<StructuredError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)] // fixtures are consumed by serde_json::from_value
    fn round_trip<T>(fixture: Value) -> T
    where
        T: serde::de::DeserializeOwned + Serialize + PartialEq + std::fmt::Debug,
    {
        let parsed: T = serde_json::from_value(fixture.clone()).expect("fixture deserializes");
        let out = serde_json::to_value(&parsed).expect("message serializes");
        assert_eq!(out, fixture, "message must round-trip unchanged");
        assert!(
            !serde_json::to_string(&parsed).unwrap().contains("null"),
            "optional fields must be omitted, not null"
        );
        parsed
    }

    #[test]
    fn user_message_round_trips() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1_730_000_000_000_i64},
            "type": "user",
            "text": "read the file",
            "files": [{
                "data": "aGk=",
                "mime": "text/plain",
                "source": {"type": "inline"},
                "name": "a.txt"
            }],
            "agents": [{"name": "build"}],
            "skills": [{"id": "skl_1", "name": "rust", "text": "use rust"}]
        });
        let message: Message = round_trip(fixture);
        assert_eq!(message.kind(), "user");
        assert!(matches!(message.body, MessageBody::User(_)));
    }

    #[test]
    fn assistant_message_with_text_and_tool_parts_round_trips() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1, "streamed": 2, "completed": 3},
            "type": "assistant",
            "agent": "build",
            "model": {"id": "claude-sonnet-4", "providerID": "anthropic"},
            "content": [
                {"type": "text", "text": "reading"},
                {
                    "type": "tool",
                    "id": "call_1",
                    "name": "read",
                    "executed": true,
                    "state": {
                        "status": "completed",
                        "input": {"path": "a.txt"},
                        "content": [{"type": "text", "text": "hi"}],
                        "metadata": {"lines": 1}
                    },
                    "time": {"created": 4, "ran": 5, "completed": 6}
                }
            ],
            "finish": "tool-calls",
            "cost": 0.0007,
            "tokens": {
                "input": 10,
                "output": 5,
                "reasoning": 2,
                "cache": {"read": 3, "write": 1}
            }
        });
        let message: Message = round_trip(fixture);
        assert_eq!(message.kind(), "assistant");
        let MessageBody::Assistant(assistant) = message.body else {
            panic!("expected assistant message");
        };
        assert_eq!(assistant.content.len(), 2);
        assert!(matches!(assistant.content[0], AssistantPart::Text(_)));
        let AssistantPart::Tool(tool) = &assistant.content[1] else {
            panic!("expected tool part");
        };
        assert!(matches!(tool.state, ToolState::Completed { .. }));
    }

    #[test]
    fn every_tool_state_round_trips() {
        let fixtures = [
            serde_json::json!({"status": "streaming", "input": "{\"path\":"}),
            serde_json::json!({
                "status": "running",
                "input": {"path": "a.txt"},
                "metadata": {"progress": 0.5}
            }),
            serde_json::json!({
                "status": "completed",
                "input": {"path": "a.txt"},
                "content": [{"type": "text", "text": "hi"}],
                "metadata": {"lines": 1}
            }),
            serde_json::json!({
                "status": "error",
                "input": {"path": "missing"},
                "error": {"type": "ToolError", "message": "missing file"},
                "content": [{"type": "text", "text": "E_NOENT"}]
            }),
        ];
        for fixture in fixtures {
            let state: ToolState = round_trip(fixture.clone());
            let status = serde_json::to_value(&state).unwrap()["status"].clone();
            assert_eq!(status, fixture["status"]);
        }
    }

    #[test]
    fn completed_tool_state_omits_empty_metadata() {
        let state = ToolState::Completed {
            input: Map::new(),
            content: vec![ToolContent::Text {
                text: "ok".to_owned(),
            }],
            metadata: None,
        };
        let text = serde_json::to_string(&state).unwrap();
        assert!(!text.contains("metadata"));
        assert!(!text.contains("null"));
    }

    #[test]
    fn file_tool_content_round_trips() {
        let fixture = serde_json::json!({"type": "file", "uri": "media:abc", "mime": "image/png"});
        round_trip::<ToolContent>(fixture);
    }

    #[test]
    fn compaction_message_discriminates_on_status() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1},
            "type": "compaction",
            "status": "completed",
            "reason": "auto",
            "summary": "collapsed"
        });
        let message: Message = round_trip(fixture);
        assert_eq!(message.kind(), "compaction");
    }

    #[test]
    fn idle_message_round_trips() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1},
            "type": "idle",
            "outcome": "interrupted"
        });
        let message: Message = round_trip(fixture);
        assert_eq!(message.kind(), "idle");
    }

    #[test]
    fn location_switched_uses_camel_case_ids() {
        let fixture = serde_json::json!({
            "id": "msg_0123456789abABCDEFGHIJKLMN",
            "time": {"created": 1},
            "type": "location-switched",
            "location": {"directory": "/work"},
            "projectID": "prj_0123456789abABCDEFGHIJKLMN",
            "subpath": "crates",
            "previous": {"location": {"directory": "/old"}}
        });
        round_trip::<Message>(fixture);
    }
}
