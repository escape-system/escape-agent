//! Ends limits, the prompt-only action and the stored firing outcome of a schedule.

use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// The argument form of `Schedule.Ends`: stop at the first limit reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleEnds {
    /// A duration from creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// An RFC 3339 instant or epoch-millisecond number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// The number of firings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

/// The wire `Schedule.Action` object. The only action is a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScheduleAction {
    /// Send `prompt` to the schedule's session.
    Prompt { prompt: String },
}

impl ScheduleAction {
    pub(super) fn validate(&self) -> Result<(), StoreError> {
        match self {
            Self::Prompt { prompt } if prompt.is_empty() => Err(StoreError::Invalid(
                "schedule prompt must not be empty".to_owned(),
            )),
            Self::Prompt { .. } => Ok(()),
        }
    }
}

/// The wire `Schedule.Outcome` of the most recent firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleOutcome {
    /// Fired on time.
    Fired,
    /// Fired after the due instant by a minute or more.
    Late,
    /// A prompt from an earlier firing was still pending.
    Skipped,
    /// The session's configuration has schedules disabled.
    Disabled,
    /// The prompt could not be delivered.
    Failed,
}

impl ScheduleOutcome {
    /// The stored lower-case wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fired => "fired",
            Self::Late => "late",
            Self::Skipped => "skipped",
            Self::Disabled => "disabled",
            Self::Failed => "failed",
        }
    }
}
