//! Durable schedules: a prompt that wakes a session once, at an instant, every
//! interval, or on a cron expression.
//!
//! The wire shapes come from `Schedule.*` in
//! `packages/schema/src/schedule.ts`:
//!
//! - [`ScheduleWhen`] is the tagged `Schedule.When` object.
//! - [`ScheduleAction`] is the tagged `Schedule.Action` object.
//! - [`ScheduleEnds`] is `Schedule.Ends`, the argument form of the limits.
//! - [`Store::create_schedule`] and friends answer the exact `Schedule.Info`
//!   object ([`serde_json::Value`]), with `sessionID`, `name`, `nextDueAt`,
//!   `endsAt`, `remainingRuns`, `lastFiredAt` and `lastOutcome` omitted when
//!   they are absent.
//!
//! Only UTC and numeric `+HH:MM` / `-HH:MM` zones are evaluated. An IANA name is
//! rejected with [`StoreError::Invalid`] rather than silently interpreted as
//! UTC. Durations, instants and five-field cron expressions are parsed locally
//! with integer UTC arithmetic, so a malformed value is an error and never a
//! panic.
//!
//! A repeating trigger keeps its phase: a firing that is several slots late
//! moves to the first future slot instead of firing the missed slots in a
//! burst. `ends` limits such a schedule by an instant, a duration from creation,
//! or a number of runs, whichever is reached first.

mod cron;
mod ends;
mod model;
mod repo;
mod row;
mod when;
mod zone;

pub use ends::{ScheduleAction, ScheduleEnds, ScheduleOutcome};
pub use model::{NewSchedule, SchedulePatch};
pub use when::{ScheduleWhen, duration_ms};

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const MINUTES_PER_DAY: i64 = 1_440;
/// How far ahead a cron search looks before deciding the expression never
/// fires. Twelve years covers the longest Gregorian leap-day gap after 2100.
const CRON_HORIZON_DAYS: i64 = 366 * 12;

/// One stored row before it is shaped for the wire.
struct ScheduleRow {
    id: String,
    session_id: String,
    name: Option<String>,
    when_json: String,
    tz: String,
    action_json: String,
    status: String,
    next_due_at: Option<i64>,
    ends_at: Option<i64>,
    remaining_runs: Option<i64>,
    run_count: i64,
    last_fired_at: Option<i64>,
    last_outcome: Option<String>,
    time_created: i64,
    time_updated: i64,
}
