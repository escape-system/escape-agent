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

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::db::{self, Store, in_tx, now_ms};
use crate::error::StoreError;
use crate::session::ensure_session;

const SCHEDULE_COLUMNS: &str = "id, session_id, name, when_json, tz, action_json, status, \
     next_due_at, ends_at, remaining_runs, run_count, last_fired_at, last_outcome, \
     time_created, time_updated";

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const MINUTES_PER_DAY: i64 = 1_440;
/// Repeating schedules may not fire faster than once a minute.
const MIN_INTERVAL_MS: i64 = MINUTE_MS;
/// How far ahead a cron search looks before deciding the expression never
/// fires. Twelve years covers the longest Gregorian leap-day gap after 2100.
const CRON_HORIZON_DAYS: i64 = 366 * 12;

/// A parsed duration in milliseconds.
///
/// Accepts a plain integer (seconds) or one or more `<integer> <unit>` pairs,
/// with `ms`, seconds, minutes, hours, days and weeks:
/// `"90 seconds"`, `"5 minutes"`, `"2 hours"`, `"1 day"`, `"1 hour 30 minutes"`.
///
/// # Errors
///
/// Returns [`StoreError::Invalid`] for a missing, negative, zero or overflowing
/// duration, or an unknown unit.
pub fn duration_ms(value: &str) -> Result<i64, StoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_duration(value));
    }
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() == 1 {
        let seconds: i64 = parts[0].parse().map_err(|_| invalid_duration(value))?;
        return positive(
            seconds
                .checked_mul(1_000)
                .ok_or_else(|| invalid_duration(value))?,
            value,
        );
    }
    let mut total = 0_i64;
    for pair in parts.chunks(2) {
        if pair.len() != 2 {
            return Err(invalid_duration(value));
        }
        let amount: i64 = pair[0].parse().map_err(|_| invalid_duration(value))?;
        let unit = pair[1];
        if amount <= 0 {
            return Err(invalid_duration(value));
        }
        let unit_ms = match unit.to_ascii_lowercase().as_str() {
            "ms" | "millisecond" | "milliseconds" => 1,
            "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
            "m" | "min" | "mins" | "minute" | "minutes" => MINUTE_MS,
            "h" | "hr" | "hrs" | "hour" | "hours" => HOUR_MS,
            "d" | "day" | "days" => DAY_MS,
            "w" | "week" | "weeks" => 7 * DAY_MS,
            _ => return Err(invalid_duration(value)),
        };
        let part = amount
            .checked_mul(unit_ms)
            .ok_or_else(|| invalid_duration(value))?;
        total = total
            .checked_add(part)
            .ok_or_else(|| invalid_duration(value))?;
    }
    positive(total, value)
}

fn positive(millis: i64, value: &str) -> Result<i64, StoreError> {
    if millis > 0 {
        Ok(millis)
    } else {
        Err(invalid_duration(value))
    }
}

fn invalid_duration(value: &str) -> StoreError {
    StoreError::Invalid(format!("invalid duration: {value:?}"))
}

/// The wire `Schedule.When` object.
///
/// Serde uses the tag `type` and lower-case variant names, so this round-trips
/// as `{"type":"in","in":"5 minutes"}`,
/// `{"type":"at","at":"2026-10-27T09:00:00Z"}`,
/// `{"type":"every","every":"2 hours"}` or
/// `{"type":"cron","cron":"*/5 * * * *"}`, each with an optional `tz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ScheduleWhen {
    /// One-shot: the duration from creation.
    In {
        #[serde(rename = "in")]
        r#in: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// One-shot: an RFC 3339 instant or an epoch-millisecond number.
    At {
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Repeating: the fixed interval.
    Every {
        every: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
    /// Repeating: a five-field cron expression (`minute hour day month weekday`).
    Cron {
        cron: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tz: Option<String>,
    },
}

impl ScheduleWhen {
    /// The zone supplied with the trigger, if any.
    #[must_use]
    pub fn tz(&self) -> Option<&str> {
        match self {
            Self::In { tz, .. }
            | Self::At { tz, .. }
            | Self::Every { tz, .. }
            | Self::Cron { tz, .. } => tz.as_deref(),
        }
    }

    /// The first instant this trigger fires, strictly after `now_ms`.
    ///
    /// `in` and `every` are relative to `now_ms`; `at` must be in the future;
    /// `cron` is evaluated in the trigger's zone (UTC when absent). The zone is
    /// validated for every variant, including the relative triggers whose math
    /// does not use it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a malformed duration, instant, cron
    /// expression or zone, an `at` in the past, an `every` below one minute, or
    /// a cron expression that never matches.
    pub fn first_due(&self, now_ms: i64) -> Result<i64, StoreError> {
        match self {
            Self::In { r#in, .. } => {
                // A relative trigger has no wall-clock component, but an
                // explicit zone is still validated rather than ignored.
                self.zone()?;
                add(now_ms, duration_ms(r#in)?)
            }
            Self::At { at, .. } => {
                let (_, offset) = self.zone()?;
                let instant = parse_instant(at, offset)?;
                if instant <= now_ms {
                    return Err(StoreError::Invalid(format!("time is in the past: {at:?}")));
                }
                Ok(instant)
            }
            Self::Every { every, .. } => {
                self.zone()?;
                let every = duration_ms(every)?;
                if every < MIN_INTERVAL_MS {
                    return Err(StoreError::Invalid(
                        "every must be at least 1 minute".to_owned(),
                    ));
                }
                add(now_ms, every)
            }
            Self::Cron { cron, .. } => {
                let (_, offset) = self.zone()?;
                next_cron_after(cron, offset, now_ms)?
                    .ok_or_else(|| StoreError::Invalid(format!("cron never fires: {cron:?}")))
            }
        }
    }

    /// The next firing strictly after `now_ms`, given the previous due instant.
    ///
    /// Returns `None` for the one-shot `in` and `at` triggers. `every` keeps the
    /// phase from `previous_due` and skips slots already missed; `cron` ignores
    /// `previous_due`. The zone is validated for every variant.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] for a malformed duration, cron expression or
    /// zone.
    pub fn next_after(&self, now_ms: i64, previous_due: i64) -> Result<Option<i64>, StoreError> {
        match self {
            Self::In { .. } | Self::At { .. } => {
                self.zone()?;
                Ok(None)
            }
            Self::Every { every, .. } => {
                self.zone()?;
                let every = duration_ms(every)?;
                if previous_due > now_ms {
                    return Ok(Some(previous_due));
                }
                let elapsed = now_ms
                    .checked_sub(previous_due)
                    .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))?;
                let missed = elapsed.div_euclid(every) + 1;
                let step = missed
                    .checked_mul(every)
                    .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))?;
                Ok(Some(add(previous_due, step)?))
            }
            Self::Cron { cron, .. } => {
                let (_, offset) = self.zone()?;
                next_cron_after(cron, offset, now_ms)
            }
        }
    }

    /// Replaces the zone with the resolved canonical form.
    #[must_use]
    fn with_tz(self, tz: &str) -> Self {
        let zone = Some(tz.to_owned());
        match self {
            Self::In { r#in, .. } => Self::In { r#in, tz: zone },
            Self::At { at, .. } => Self::At { at, tz: zone },
            Self::Every { every, .. } => Self::Every { every, tz: zone },
            Self::Cron { cron, .. } => Self::Cron { cron, tz: zone },
        }
    }

    fn zone(&self) -> Result<(String, i64), StoreError> {
        zone_offset_ms(self.tz())
    }
}

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
    fn validate(&self) -> Result<(), StoreError> {
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

/// Input for [`Store::create_schedule`], built from `Schedule.CreatePayload`.
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

/// Fields [`Store::update_schedule`] may change. Omitted fields are kept.
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

impl ScheduleRow {
    fn read(row: &rusqlite::Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.get(0)?,
            session_id: row.get(1)?,
            name: row.get(2)?,
            when_json: row.get(3)?,
            tz: row.get(4)?,
            action_json: row.get(5)?,
            status: row.get(6)?,
            next_due_at: row.get(7)?,
            ends_at: row.get(8)?,
            remaining_runs: row.get(9)?,
            run_count: row.get(10)?,
            last_fired_at: row.get(11)?,
            last_outcome: row.get(12)?,
            time_created: row.get(13)?,
            time_updated: row.get(14)?,
        })
    }

    fn when(&self) -> Result<ScheduleWhen, StoreError> {
        serde_json::from_str(&self.when_json).map_err(|error| {
            StoreError::Corrupt(format!("schedule {} has invalid when: {error}", self.id))
        })
    }

    fn action(&self) -> Result<ScheduleAction, StoreError> {
        serde_json::from_str(&self.action_json).map_err(|error| {
            StoreError::Corrupt(format!("schedule {} has invalid action: {error}", self.id))
        })
    }

    /// The exact wire `Schedule.Info` object.
    fn info(&self) -> Result<Value, StoreError> {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String(self.id.clone()));
        map.insert(
            "sessionID".to_owned(),
            Value::String(self.session_id.clone()),
        );
        if let Some(name) = self.name.as_deref() {
            map.insert("name".to_owned(), Value::String(name.to_owned()));
        }
        map.insert("when".to_owned(), db::decode_json(&self.when_json)?);
        map.insert("tz".to_owned(), Value::String(self.tz.clone()));
        map.insert("action".to_owned(), db::decode_json(&self.action_json)?);
        map.insert("status".to_owned(), Value::String(self.status.clone()));
        if let Some(next_due_at) = self.next_due_at {
            map.insert("nextDueAt".to_owned(), json!(next_due_at));
        }
        if let Some(ends_at) = self.ends_at {
            map.insert("endsAt".to_owned(), json!(ends_at));
        }
        if let Some(remaining_runs) = self.remaining_runs {
            map.insert("remainingRuns".to_owned(), json!(remaining_runs));
        }
        map.insert("runCount".to_owned(), json!(self.run_count));
        if let Some(last_fired_at) = self.last_fired_at {
            map.insert("lastFiredAt".to_owned(), json!(last_fired_at));
        }
        if let Some(last_outcome) = self.last_outcome.as_deref() {
            map.insert(
                "lastOutcome".to_owned(),
                Value::String(last_outcome.to_owned()),
            );
        }
        map.insert(
            "time".to_owned(),
            json!({ "created": self.time_created, "updated": self.time_updated }),
        );
        Ok(Value::Object(map))
    }
}

impl Store {
    /// Creates a schedule and returns its wire `Schedule.Info`.
    ///
    /// The trigger is validated and `nextDueAt`, `endsAt` and `remainingRuns`
    /// are computed from `when`/`ends` before the row is written.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] for a missing session; [`StoreError::Invalid`]
    /// for a bad trigger, prompt or end limit, or an end before the first
    /// firing.
    pub async fn create_schedule(&self, new: NewSchedule) -> Result<Value, StoreError> {
        let NewSchedule {
            session_id,
            name,
            when,
            ends,
            action,
            time_created,
        } = new;
        action.validate()?;
        let (tz, _) = when.zone()?;
        let when = when.with_tz(&tz);
        let next_due_at = when.first_due(time_created)?;
        let (ends_at, remaining_runs) = ends_values(ends.as_ref(), &tz, time_created)?;
        if let Some(ends_at) = ends_at
            && next_due_at > ends_at
        {
            return Err(StoreError::Invalid(
                "the schedule ends before it first fires".to_owned(),
            ));
        }
        let when_json = serde_json::to_string(&when)?;
        let action_json = serde_json::to_string(&action)?;
        let id = db::generate_id("sch_");
        let id_for_read = id.clone();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                transaction.execute(
                    "INSERT INTO schedule
                        (id, session_id, name, when_json, tz, action_json, status,
                         next_due_at, ends_at, remaining_runs, run_count,
                         time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, 0, ?10, ?10)",
                    params![
                        &id,
                        &session_id,
                        name.as_deref(),
                        &when_json,
                        &tz,
                        &action_json,
                        next_due_at,
                        ends_at,
                        remaining_runs,
                        time_created,
                    ],
                )?;
                let row = row_for(transaction, &id_for_read)?
                    .ok_or_else(|| StoreError::NotFound(format!("schedule {id_for_read}")))?;
                row.info()
            })
        })
        .await
    }

    /// Lists schedules oldest first, optionally restricted to one session.
    pub async fn list_schedules(&self, session_id: Option<&str>) -> Result<Vec<Value>, StoreError> {
        let session_id = session_id.map(str::to_owned);
        self.read(move |connection| {
            let sql = if session_id.is_some() {
                format!(
                    "SELECT {SCHEDULE_COLUMNS} FROM schedule WHERE session_id = ?1
                     ORDER BY time_created ASC, id ASC"
                )
            } else {
                format!(
                    "SELECT {SCHEDULE_COLUMNS} FROM schedule
                     ORDER BY time_created ASC, id ASC"
                )
            };
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(rusqlite::params_from_iter(
                session_id.iter().map(String::as_str),
            ))?;
            let mut schedules = Vec::new();
            while let Some(row) = rows.next()? {
                schedules.push(ScheduleRow::read(row)?.info()?);
            }
            Ok(schedules)
        })
        .await
    }

    /// Loads one schedule as its wire `Schedule.Info`.
    pub async fn get_schedule(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| match row_for(connection, &id)? {
            Some(row) => Ok(Some(row.info()?)),
            None => Ok(None),
        })
        .await
    }

    /// Applies a patch and returns the updated `Schedule.Info`.
    ///
    /// A new `when` recomputes `nextDueAt`; new `ends` recompute `endsAt` and
    /// `remainingRuns`. A schedule that can no longer fire (a one-shot already
    /// spent, `remainingRuns` at zero, or the next firing past `endsAt`) becomes
    /// `done`. Returns `None` when the id is unknown.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] when the replacement payload is malformed, and
    /// [`StoreError::Corrupt`] if a stored trigger cannot be decoded.
    pub async fn update_schedule(
        &self,
        id: &str,
        patch: SchedulePatch,
    ) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        let now = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(row) = row_for(transaction, &id)? else {
                    return Ok(None);
                };
                let when_changed = patch.when.is_some();
                let (when, tz) = match patch.when {
                    Some(when) => {
                        let requested = when.tz().unwrap_or(row.tz.as_str()).to_owned();
                        let (canonical, _) = zone_offset_ms(Some(requested.as_str()))?;
                        (when.with_tz(&canonical), canonical)
                    }
                    None => (row.when()?, row.tz.clone()),
                };
                let due = if when_changed {
                    Some(when.first_due(now)?)
                } else {
                    row.next_due_at
                };
                let (ends_at, remaining_runs) = match patch.ends.as_ref() {
                    Some(ends) => ends_values(Some(ends), &tz, now)?,
                    None => (row.ends_at, row.remaining_runs),
                };
                let action = match patch.prompt {
                    Some(prompt) => {
                        let action = ScheduleAction::Prompt { prompt };
                        action.validate()?;
                        action
                    }
                    None => row.action()?,
                };
                let name = patch.name.or_else(|| row.name.clone());
                let finished = due.is_none()
                    || remaining_runs.is_some_and(|remaining| remaining <= 0)
                    || matches!((due, ends_at), (Some(due), Some(ends)) if due > ends);
                let (next_due_at, status) = if finished {
                    (None, "done".to_owned())
                } else if when_changed {
                    let status = if row.status == "paused" {
                        "paused"
                    } else {
                        "active"
                    };
                    (due, status.to_owned())
                } else {
                    (due, row.status.clone())
                };
                let when_json = serde_json::to_string(&when)?;
                let action_json = serde_json::to_string(&action)?;
                transaction.execute(
                    "UPDATE schedule
                     SET name = ?2, when_json = ?3, tz = ?4, action_json = ?5,
                         status = ?6, next_due_at = ?7, ends_at = ?8,
                         remaining_runs = ?9, time_updated = ?10
                     WHERE id = ?1",
                    params![
                        &id,
                        name.as_deref(),
                        &when_json,
                        &tz,
                        &action_json,
                        &status,
                        next_due_at,
                        ends_at,
                        remaining_runs,
                        now,
                    ],
                )?;
                let updated = row_for(transaction, &id)?
                    .ok_or_else(|| StoreError::NotFound(format!("schedule {id}")))?;
                Ok(Some(updated.info()?))
            })
        })
        .await
    }

    /// Deletes a schedule. `false` when the id was already gone.
    pub async fn delete_schedule(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            Ok(connection.execute("DELETE FROM schedule WHERE id = ?1", [&id])? > 0)
        })
        .await
    }

    /// Pauses an active schedule. Other states are returned unchanged.
    ///
    /// Returns `None` when the id is unknown.
    pub async fn pause_schedule(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        let now = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(row) = row_for(transaction, &id)? else {
                    return Ok(None);
                };
                if row.status != "active" {
                    return Ok(Some(row.info()?));
                }
                transaction.execute(
                    "UPDATE schedule SET status = 'paused', time_updated = ?2 WHERE id = ?1",
                    params![&id, now],
                )?;
                let updated = row_for(transaction, &id)?
                    .ok_or_else(|| StoreError::NotFound(format!("schedule {id}")))?;
                Ok(Some(updated.info()?))
            })
        })
        .await
    }

    /// Resumes a paused schedule. Other states are returned unchanged.
    ///
    /// A one-shot that fell due while paused keeps its due instant and fires
    /// late once; a repeating schedule moves to its next future slot. Returns
    /// `None` when the id is unknown.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if a stored trigger cannot be decoded, and
    /// [`StoreError::Invalid`] if it cannot be evaluated.
    pub async fn resume_schedule(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        let now = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(row) = row_for(transaction, &id)? else {
                    return Ok(None);
                };
                if row.status != "paused" {
                    return Ok(Some(row.info()?));
                }
                let when = row.when()?;
                let next_due_at = match (&when, row.next_due_at) {
                    (ScheduleWhen::In { .. } | ScheduleWhen::At { .. }, due) => due,
                    (_, Some(due)) if due <= now => when.next_after(now, due)?,
                    (_, due) => due,
                };
                let finished = next_due_at.is_none()
                    || row.remaining_runs.is_some_and(|remaining| remaining <= 0)
                    || matches!(
                        (next_due_at, row.ends_at),
                        (Some(due), Some(ends)) if due > ends
                    );
                let (next_due_at, status) = if finished {
                    (None, "done")
                } else {
                    (next_due_at, "active")
                };
                transaction.execute(
                    "UPDATE schedule
                     SET status = ?2, next_due_at = ?3, time_updated = ?4
                     WHERE id = ?1",
                    params![&id, status, next_due_at, now],
                )?;
                let updated = row_for(transaction, &id)?
                    .ok_or_else(|| StoreError::NotFound(format!("schedule {id}")))?;
                Ok(Some(updated.info()?))
            })
        })
        .await
    }

    /// Active schedules whose `nextDueAt` is at or before `now_ms`, earliest
    /// first. `limit` is capped at the store's read limit (200).
    pub async fn due_schedules(&self, now_ms: i64, limit: usize) -> Result<Vec<Value>, StoreError> {
        let limit = i64::try_from(limit.min(db::MAX_LIMIT)).unwrap_or(i64::MAX);
        self.read(move |connection| {
            let sql = format!(
                "SELECT {SCHEDULE_COLUMNS} FROM schedule
                 WHERE status = 'active'
                   AND next_due_at IS NOT NULL
                   AND next_due_at <= ?1
                 ORDER BY next_due_at ASC, id ASC
                 LIMIT ?2"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params![now_ms, limit])?;
            let mut schedules = Vec::new();
            while let Some(row) = rows.next()? {
                schedules.push(ScheduleRow::read(row)?.info()?);
            }
            Ok(schedules)
        })
        .await
    }

    /// Records one firing and returns the updated `Schedule.Info`.
    ///
    /// A `Fired` or `Late` outcome decrements `remainingRuns`; a skipped,
    /// disabled or failed attempt records the outcome without spending a run.
    /// When the count reaches zero, when the passed `next_due_at` is past
    /// `endsAt`, or when `next_due_at` is `None`, the schedule becomes `done`
    /// with no `nextDueAt`. Returns `None` when the id is unknown.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] if the stored row cannot be shaped.
    pub async fn mark_schedule_fired(
        &self,
        id: &str,
        outcome: ScheduleOutcome,
        next_due_at: Option<i64>,
        run_count: i64,
    ) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        let now = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let Some(row) = row_for(transaction, &id)? else {
                    return Ok(None);
                };
                let consumed = matches!(outcome, ScheduleOutcome::Fired | ScheduleOutcome::Late);
                let remaining_runs = if consumed {
                    row.remaining_runs
                        .map(|remaining| remaining.saturating_sub(1).max(0))
                } else {
                    row.remaining_runs
                };
                let exhausted = remaining_runs == Some(0);
                let past_end = matches!(
                    (next_due_at, row.ends_at),
                    (Some(due), Some(ends)) if due > ends
                );
                let next_due_at = if exhausted || past_end {
                    None
                } else {
                    next_due_at
                };
                let status = if next_due_at.is_none() {
                    "done"
                } else {
                    row.status.as_str()
                };
                let last_fired_at = if consumed {
                    Some(now)
                } else {
                    row.last_fired_at
                };
                transaction.execute(
                    "UPDATE schedule
                     SET last_fired_at = ?2, last_outcome = ?3, next_due_at = ?4,
                         run_count = ?5, remaining_runs = ?6, status = ?7,
                         time_updated = ?8
                     WHERE id = ?1",
                    params![
                        &id,
                        last_fired_at,
                        outcome.as_str(),
                        next_due_at,
                        run_count.max(0),
                        remaining_runs,
                        status,
                        now,
                    ],
                )?;
                let updated = row_for(transaction, &id)?
                    .ok_or_else(|| StoreError::NotFound(format!("schedule {id}")))?;
                Ok(Some(updated.info()?))
            })
        })
        .await
    }
}

fn row_for(connection: &rusqlite::Connection, id: &str) -> Result<Option<ScheduleRow>, StoreError> {
    let sql = format!("SELECT {SCHEDULE_COLUMNS} FROM schedule WHERE id = ?1");
    connection
        .query_row(&sql, [id], |row| Ok(ScheduleRow::read(row)))
        .optional()?
        .transpose()
}

/// Resolves an end limit to `(endsAt, remainingRuns)`.
fn ends_values(
    ends: Option<&ScheduleEnds>,
    tz: &str,
    now_ms: i64,
) -> Result<(Option<i64>, Option<i64>), StoreError> {
    let Some(ends) = ends else {
        return Ok((None, None));
    };
    let (_, offset) = zone_offset_ms(Some(tz))?;
    let after = match ends.after.as_deref() {
        Some(after) => Some(add(now_ms, duration_ms(after)?)?),
        None => None,
    };
    let at = match ends.at.as_deref() {
        Some(at) => Some(parse_instant(at, offset)?),
        None => None,
    };
    let ends_at = match (after, at) {
        (Some(after), Some(at)) => Some(after.min(at)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    let remaining_runs = match ends.times {
        Some(0) => {
            return Err(StoreError::Invalid(
                "ends.times must be at least 1".to_owned(),
            ));
        }
        Some(times) => Some(i64::from(times)),
        None => None,
    };
    Ok((ends_at, remaining_runs))
}

fn add(instant: i64, delta: i64) -> Result<i64, StoreError> {
    instant
        .checked_add(delta)
        .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))
}

/// Canonical zone string and its fixed offset in milliseconds.
///
/// Accepts `UTC` (the default) and `+HH:MM` / `-HH:MM`. Everything else is an
/// error: this store has no IANA database.
fn zone_offset_ms(tz: Option<&str>) -> Result<(String, i64), StoreError> {
    let raw = match tz {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(StoreError::Invalid(format!(
                    "unsupported time zone: {value:?}"
                )));
            }
            value
        }
        None => "UTC",
    };
    if raw.eq_ignore_ascii_case("utc") || raw.eq_ignore_ascii_case("z") {
        return Ok(("UTC".to_owned(), 0));
    }
    let (sign, rest) = match raw.as_bytes().first() {
        Some(b'+') => (1_i64, &raw[1..]),
        Some(b'-') => (-1_i64, &raw[1..]),
        _ => {
            return Err(StoreError::Invalid(format!(
                "unsupported time zone: {raw:?}"
            )));
        }
    };
    let Some((hours, minutes)) = rest.split_once(':') else {
        return Err(StoreError::Invalid(format!(
            "unsupported time zone: {raw:?}"
        )));
    };
    if hours.len() != 2
        || minutes.len() != 2
        || !hours.bytes().all(|byte| byte.is_ascii_digit())
        || !minutes.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(StoreError::Invalid(format!(
            "unsupported time zone: {raw:?}"
        )));
    }
    let hours: i64 = hours
        .parse()
        .map_err(|_| StoreError::Invalid(format!("unsupported time zone: {raw:?}")))?;
    let minutes: i64 = minutes
        .parse()
        .map_err(|_| StoreError::Invalid(format!("unsupported time zone: {raw:?}")))?;
    if hours > 23 || minutes > 59 {
        return Err(StoreError::Invalid(format!(
            "unsupported time zone: {raw:?}"
        )));
    }
    let mark = if sign > 0 { '+' } else { '-' };
    Ok((
        format!("{mark}{hours:02}:{minutes:02}"),
        sign * (hours * HOUR_MS + minutes * MINUTE_MS),
    ))
}

/// Parses an RFC 3339 instant, a wall-clock instant in `default_offset` when no
/// offset is written, or a numeric epoch-millisecond value.
fn parse_instant(value: &str, default_offset: i64) -> Result<i64, StoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid_instant(value));
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    if !unsigned.is_empty() && unsigned.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<i64>().map_err(|_| invalid_instant(value));
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    let year = take_digits(bytes, &mut index, 4).ok_or_else(|| invalid_instant(value))?;
    expect(bytes, &mut index, b'-').ok_or_else(|| invalid_instant(value))?;
    let month = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    expect(bytes, &mut index, b'-').ok_or_else(|| invalid_instant(value))?;
    let day = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    if !matches!(bytes.get(index), Some(b'T' | b't' | b' ')) {
        return Err(invalid_instant(value));
    }
    index += 1;
    let hour = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    expect(bytes, &mut index, b':').ok_or_else(|| invalid_instant(value))?;
    let minute = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
    let mut second = 0;
    let mut fraction = 0;
    if bytes.get(index) == Some(&b':') {
        index += 1;
        second = take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            let digits = &bytes[start..index];
            if digits.is_empty() {
                return Err(invalid_instant(value));
            }
            let mut millis = 0_i64;
            for (position, digit) in digits.iter().take(3).enumerate() {
                millis += i64::from(*digit - b'0') * [100_i64, 10, 1][position];
            }
            fraction = millis;
        }
    }
    let offset = if index == bytes.len() {
        default_offset
    } else {
        match bytes[index] {
            b'Z' | b'z' => {
                index += 1;
                0
            }
            sign @ (b'+' | b'-') => {
                index += 1;
                let hours =
                    take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
                expect(bytes, &mut index, b':').ok_or_else(|| invalid_instant(value))?;
                let minutes =
                    take_digits(bytes, &mut index, 2).ok_or_else(|| invalid_instant(value))?;
                if hours > 23 || minutes > 59 {
                    return Err(invalid_instant(value));
                }
                let magnitude = hours * HOUR_MS + minutes * MINUTE_MS;
                if sign == b'-' { -magnitude } else { magnitude }
            }
            _ => return Err(invalid_instant(value)),
        }
    };
    if index != bytes.len() {
        return Err(invalid_instant(value));
    }
    if !(1..=12).contains(&month)
        || day < 1
        || day > i64::from(days_in_month(year, u32::try_from(month).unwrap_or(1)))
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(invalid_instant(value));
    }
    let days = days_from_civil(
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    );
    let local = days
        .checked_mul(DAY_MS)
        .and_then(|value| {
            value.checked_add(hour * HOUR_MS + minute * MINUTE_MS + second * 1_000 + fraction)
        })
        .ok_or_else(|| invalid_instant(value))?;
    local
        .checked_sub(offset)
        .ok_or_else(|| invalid_instant(value))
}

fn invalid_instant(value: &str) -> StoreError {
    StoreError::Invalid(format!("invalid time: {value:?}"))
}

fn take_digits(bytes: &[u8], index: &mut usize, count: usize) -> Option<i64> {
    let end = index.checked_add(count)?;
    let slice = bytes.get(*index..end)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *index = end;
    Some(
        slice
            .iter()
            .fold(0_i64, |value, byte| value * 10 + i64::from(*byte - b'0')),
    )
}

fn expect(bytes: &[u8], index: &mut usize, expected: u8) -> Option<()> {
    if bytes.get(*index) == Some(&expected) {
        *index += 1;
        Some(())
    } else {
        None
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let day = i64::from(day);
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date for a day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}

/// A parsed five-field cron expression, evaluated in a fixed-offset zone.
struct CronSpec {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    days_any: bool,
    weekdays_any: bool,
}

impl CronSpec {
    fn parse(expression: &str) -> Result<Self, StoreError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(invalid_cron(expression));
        }
        let minutes = cron_field(fields[0], 0, 59, expression)?;
        let hours = cron_field(fields[1], 0, 23, expression)?;
        let days = cron_field(fields[2], 1, 31, expression)?;
        let months = cron_field(fields[3], 1, 12, expression)?;
        let weekdays = normalize_weekdays(cron_field(fields[4], 0, 7, expression)?);
        let days_any = days.len() == 31;
        let weekdays_any = weekdays.len() == 7;
        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays,
            days_any,
            weekdays_any,
        })
    }
}

fn invalid_cron(expression: &str) -> StoreError {
    StoreError::Invalid(format!("invalid cron: {expression:?}"))
}

/// Parses one cron field into sorted unique values.
fn cron_field(field: &str, min: u32, max: u32, expression: &str) -> Result<Vec<u32>, StoreError> {
    if field.is_empty() {
        return Err(invalid_cron(expression));
    }
    let mut values = Vec::new();
    for part in field.split(',') {
        if part.is_empty() {
            return Err(invalid_cron(expression));
        }
        if part == "*" {
            values.extend(min..=max);
        } else if let Some(step) = part.strip_prefix("*/") {
            let step: u32 = step.parse().map_err(|_| invalid_cron(expression))?;
            let step = usize::try_from(step).map_err(|_| invalid_cron(expression))?;
            if step == 0 {
                return Err(invalid_cron(expression));
            }
            values.extend((min..=max).step_by(step));
        } else if let Some((start, end)) = part.split_once('-') {
            let start: u32 = start.parse().map_err(|_| invalid_cron(expression))?;
            let end: u32 = end.parse().map_err(|_| invalid_cron(expression))?;
            if start < min || end > max || start > end {
                return Err(invalid_cron(expression));
            }
            values.extend(start..=end);
        } else {
            let value: u32 = part.parse().map_err(|_| invalid_cron(expression))?;
            if !(min..=max).contains(&value) {
                return Err(invalid_cron(expression));
            }
            values.push(value);
        }
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}

/// Maps cron's `7` to Sunday and sorts the result.
fn normalize_weekdays(mut values: Vec<u32>) -> Vec<u32> {
    for value in &mut values {
        if *value == 7 {
            *value = 0;
        }
    }
    values.sort_unstable();
    values.dedup();
    values
}

/// The first cron slot strictly after `now_ms`, in the fixed-offset zone.
fn next_cron_after(
    expression: &str,
    offset_ms: i64,
    now_ms: i64,
) -> Result<Option<i64>, StoreError> {
    let spec = CronSpec::parse(expression)?;
    let local_now = now_ms
        .checked_add(offset_ms)
        .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))?;
    let first_minute = local_now.div_euclid(MINUTE_MS) + 1;
    let mut day = first_minute.div_euclid(MINUTES_PER_DAY);
    let last_day = day + CRON_HORIZON_DAYS;
    while day <= last_day {
        let (year, month, day_of_month) = civil_from_days(day);
        if year > 9_999 {
            return Ok(None);
        }
        let weekday = u32::try_from((day + 4).rem_euclid(7)).unwrap_or_default();
        let day_matches = spec.days.contains(&day_of_month);
        let weekday_matches = spec.weekdays.contains(&weekday);
        let date_matches = match (spec.days_any, spec.weekdays_any) {
            (true, true) => true,
            (true, false) => weekday_matches,
            (false, true) => day_matches,
            (false, false) => day_matches || weekday_matches,
        };
        if spec.months.contains(&month) && date_matches {
            for hour in &spec.hours {
                for minute in &spec.minutes {
                    let candidate =
                        day * MINUTES_PER_DAY + i64::from(*hour) * 60 + i64::from(*minute);
                    if candidate >= first_minute {
                        let local = candidate.checked_mul(MINUTE_MS).ok_or_else(|| {
                            StoreError::Invalid("schedule time overflows".to_owned())
                        })?;
                        return Ok(Some(local - offset_ms));
                    }
                }
            }
        }
        day += 1;
    }
    Ok(None)
}
