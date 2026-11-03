//! Wire types for the opencode HTTP/SSE protocol.
//!
//! Rules encoded here:
//! - optional fields are omitted, never `null`;
//! - timestamps are epoch milliseconds;
//! - IDs are prefixed, opaque strings;
//! - errors are tagged objects with `_tag`.
//!
//! The phase-1/2 model mirrors `packages/client/src/promise/generated/types.ts`
//! and `packages/schema/src/*` exactly; see `docs/17-interfaces.md`.

pub mod agent;
pub mod config;
pub mod error;
pub mod event;
pub mod file;
pub mod id;
pub mod inbox;
pub mod info;
pub mod location;
pub mod message;
pub mod payload;
pub mod permission;
pub mod session;

pub use agent::{AgentInfo, AgentMode, ProviderRequest};
pub use config::{ConfigDirectory, ConfigDocument, ConfigEntry, ConfigPatch};
pub use error::ApiError;
pub use event::{Durable, EventEnvelope};
pub use file::{FileDiff, FileEntry, FileKind, FileStatus};
pub use id::{
    EventId, GoalId, LaneId, MemoryId, MessageId, ObservationId, PermissionRequestId, ProjectId,
    SavedPermissionId, SessionId, message_id_from_event, project_id_from_directory,
};
pub use inbox::{Delivery, InboxItem, InboxKind};
pub use info::{ServerCapabilities, ServerInfo, ServerPaths};
pub use location::{DataEnvelope, EventLocation, LocationEnvelope, LocationRef, ProjectRef};
pub use message::{
    AgentSwitchedMessage, AssistantMessage, AssistantPart, AssistantRetry, AssistantTime,
    CompactionCompleted, CompactionFailed, CompactionMessage, CompactionReason, CompactionRunning,
    CompactionStatus, CompletedTime, ErrorResponse, FinishReason, IdleMessage,
    LocationSwitchedMessage, Message, MessageBody, MessageTime, ModelSwitchedMessage, Native,
    PreviousLocation, PromptAgentAttachment, PromptFileAttachment, PromptFileSource, PromptMention,
    PromptSkillAttachment, ReasoningPart, ShellExit, ShellMessage, ShellOutput, ShellStatus,
    SkillMessage, SnapshotInfo, StructuredError, SyntheticMessage, SystemMessage, TextPart,
    ToolContent, ToolPart, ToolState, ToolTime, UserMessage,
};
pub use payload::{
    EventPayload, InterruptReason, SessionAgentSelected, SessionArchived, SessionCompactionEnded,
    SessionCompactionFailed, SessionCompactionStarted, SessionCreated, SessionDeleted,
    SessionExecutionFailed, SessionExecutionInterrupted, SessionExecutionStarted,
    SessionExecutionSucceeded, SessionIdle, SessionInboxCancelled, SessionInboxDelivered,
    SessionInboxDeliveryChanged, SessionInboxEnqueued, SessionMetadataUpdated,
    SessionModelSelected, SessionMoved, SessionPermissions, SessionReasoningDelta,
    SessionReasoningEnded, SessionReasoningStarted, SessionRenamed, SessionStatusUpdated,
    SessionStepEnded, SessionStepFailed, SessionStepStarted, SessionStepStreamed, SessionSynthetic,
    SessionTextDelta, SessionTextEnded, SessionTextStarted, SessionToolCalled, SessionToolFailed,
    SessionToolInputDelta, SessionToolInputEnded, SessionToolInputStarted, SessionToolProgress,
    SessionToolSuccess, SessionUnarchived, SessionUsageUpdated, SessionViewed,
};
pub use permission::{
    Effect, PermissionReply, PermissionRequest, PermissionRule, PermissionSaved,
    PermissionSavedTime, PermissionSource, PermissionSourceKind,
};
pub use session::{
    CacheTokens, Cursor, ForkBoundary, ForkRef, LocationInfo, Metadata, ModelRef, Order,
    RetryAction, SessionInfo, SessionListQuery, SessionListResponse, SessionLocation,
    SessionOutcome, SessionRevert, SessionStatus, SessionTimes, Tokens,
};

/// Current Unix time in milliseconds.
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
