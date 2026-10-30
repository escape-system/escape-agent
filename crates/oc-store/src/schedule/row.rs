//! Stored schedule row and its exact wire `Schedule.Info` projection.

use serde_json::{Map, Value, json};

use super::ScheduleRow;
use super::ends::ScheduleAction;
use super::when::ScheduleWhen;
use crate::db;
use crate::error::StoreError;

impl ScheduleRow {
    pub(super) fn read(row: &rusqlite::Row<'_>) -> Result<Self, StoreError> {
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

    pub(super) fn when(&self) -> Result<ScheduleWhen, StoreError> {
        serde_json::from_str(&self.when_json).map_err(|error| {
            StoreError::Corrupt(format!("schedule {} has invalid when: {error}", self.id))
        })
    }

    pub(super) fn action(&self) -> Result<ScheduleAction, StoreError> {
        serde_json::from_str(&self.action_json).map_err(|error| {
            StoreError::Corrupt(format!("schedule {} has invalid action: {error}", self.id))
        })
    }

    /// The exact wire `Schedule.Info` object.
    pub(super) fn info(&self) -> Result<Value, StoreError> {
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
