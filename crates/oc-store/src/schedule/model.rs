//! Input models for creating and patching a schedule.

use serde::{Deserialize, Serialize};

use super::ends::{ScheduleAction, ScheduleEnds};
use super::when::ScheduleWhen;

/// Input for [`crate::Store::create_schedule`], built from `Schedule.CreatePayload`.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    /// Owning session.
    pub session_id: String,
    /// Optional display label.
    pub name: Option<String>,
    /// When it fires.
    pub when: ScheduleWhen,
    /// Optional limits.
    pub ends: Option<ScheduleEnds>,
    /// The action, always a prompt.
    pub action: ScheduleAction,
    /// Creation instant; durations are measured from here.
    pub time_created: i64,
}

/// Fields [`crate::Store::update_schedule`] may change. Omitted fields are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulePatch {
    /// Replacement label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replacement trigger; recomputes `nextDueAt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<ScheduleWhen>,
    /// Replacement limits; recomputes `endsAt` and `remainingRuns`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends: Option<ScheduleEnds>,
    /// Replacement prompt text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}
