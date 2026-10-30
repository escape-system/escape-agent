//! Schedule CRUD on `Store`, end-limit resolution and single-row lookup.

use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use super::ends::{ScheduleAction, ScheduleOutcome};
use super::model::{NewSchedule, SchedulePatch};
use super::when::{ScheduleWhen, duration_ms};
use super::zone::{parse_instant, zone_offset_ms};
use super::{ScheduleEnds, ScheduleRow};
use crate::db::{self, Store, in_tx, now_ms};
use crate::error::StoreError;
use crate::session::ensure_session;

const SCHEDULE_COLUMNS: &str = "id, session_id, name, when_json, tz, action_json, status, \
     next_due_at, ends_at, remaining_runs, run_count, last_fired_at, last_outcome, \
     time_created, time_updated";

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

pub(super) fn add(instant: i64, delta: i64) -> Result<i64, StoreError> {
    instant
        .checked_add(delta)
        .ok_or_else(|| StoreError::Invalid("schedule time overflows".to_owned()))
}
