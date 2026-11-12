//! Agent-tree rows, goals, observation strips and observer repairs
//! (`docs/08-goals-observer.md`, `docs/19-agentic-integration.md`).
//!
//! These rows are plain tables, not event-sourced: writes go through the same
//! writer transaction but never allocate a session sequence.

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use serde_json::{Map, Value, json};

use crate::db::{Store, in_tx, now_ms};
use crate::error::StoreError;

const NODE_COLUMNS: &str = "id, parent_id, root_id, depth, role, spawn_allowed, child_count, \
agent, model, time_busy, time_idle, idle_outcome, time_archived, time_suspended, idle_reason";
const GOAL_COLUMNS: &str = "id, root_id, objective, criteria, status, rounds, stalls, \
last_progress, evidence, model, time_created, time_updated";
const OBSERVATION_COLUMNS: &str = "id, root_id, session_id, tool_call_id, tool, why, expect_s, \
intervene_s, started, settled, state, outcome, parent_call, synthetic";
const REPAIR_COLUMNS: &str =
    "id, root_id, target_id, kind, reason, args_hash, attempt, time_created";

/// One row of the subagent tree view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNodeRow {
    /// Session id.
    pub id: String,
    /// Parent session, absent for a root.
    pub parent_id: Option<String>,
    /// Root of the tree this session belongs to.
    pub root_id: String,
    /// Distance from the root.
    pub depth: u32,
    /// `primary`, `subagent` or `coordinator`.
    pub role: String,
    /// Whether this session may spawn children.
    pub spawn_allowed: bool,
    /// Number of direct children recorded by the store.
    pub child_count: u32,
    /// Selected agent, falling back to `build`.
    pub agent: String,
    /// Compact `provider/id#variant` model reference.
    pub model: Option<String>,
    /// Derived lifecycle state; see [`Store::session_node`].
    pub status: String,
    /// Why the last execution ended, when the reason is durable: the stop
    /// reason for `status == "interrupted"` (`user`, …) or the step-limit
    /// grammar (`step limit reached after N steps`) for an answered turn whose
    /// budget ran out. `None` for every other outcome.
    pub idle_reason: Option<String>,
}

/// A goal to start for a root session.
#[derive(Debug, Clone)]
pub struct NewGoal {
    /// Goal id (`gol_…`).
    pub id: String,
    /// Root session the goal belongs to.
    pub root_id: String,
    /// What the session tree must achieve.
    pub objective: String,
    /// Acceptance criteria.
    pub criteria: Vec<String>,
    /// Compact model reference that owns the goal.
    pub model: Option<String>,
    /// Creation time in epoch milliseconds.
    pub time_created: i64,
}

/// Fields to change on a goal. Absent fields are left untouched.
#[derive(Debug, Clone, Default)]
pub struct GoalPatch {
    /// New status (`active`, `paused`, `blocked`, `done`, `failed`, …).
    pub status: Option<String>,
    /// Completed rounds.
    pub rounds: Option<u32>,
    /// Stalled rounds.
    pub stalls: Option<u32>,
    /// Last observed progress timestamp.
    pub last_progress: Option<i64>,
    /// Evidence ids (`tool_call_id` / `message_id`).
    pub evidence: Option<Vec<String>>,
}

/// An unsettled tool-call strip.
#[derive(Debug, Clone)]
pub struct NewObservation {
    /// Observation id (`obs_…`).
    pub id: String,
    /// Root session the strip belongs to.
    pub root_id: String,
    /// Session that issued the call.
    pub session_id: String,
    /// Provider tool-call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool: String,
    /// One sentence describing the expectation.
    pub why: String,
    /// Expected runtime in seconds.
    pub expect_s: u32,
    /// Observer intervention deadline in seconds.
    pub intervene_s: u32,
    /// Start time in epoch milliseconds.
    pub started: i64,
    /// Parent tool call for nested strips.
    pub parent_call: Option<String>,
    /// Whether the expectation was derived rather than provided.
    pub synthetic: bool,
}

/// One observer repair attempt.
#[derive(Debug, Clone)]
pub struct NewRepair {
    /// Repair id.
    pub id: String,
    /// Root session the repair belongs to.
    pub root_id: String,
    /// Target session, tool call or message.
    pub target_id: String,
    /// `retry_tool`, `cancel_tool`, `restart_subagent`, `compact_context`,
    /// `switch_model` or `repost`.
    pub kind: String,
    /// Why the observer intervened.
    pub reason: String,
    /// Hash of the repaired arguments.
    pub args_hash: String,
    /// Attempt number.
    pub attempt: u32,
    /// Creation time in epoch milliseconds.
    pub time_created: i64,
}

impl Store {
    /// Loads one subagent-tree row.
    ///
    /// `status` is derived, in order: `archived` when the session is archived;
    /// `busy` when `time_busy` is newer than `time_idle`; `suspended` when an
    /// execution claim is present; `failed`/`interrupted` when that was the
    /// last idle outcome; otherwise `idle`.
    pub async fn session_node(&self, id: &str) -> Result<Option<SessionNodeRow>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!("SELECT {NODE_COLUMNS} FROM session_v2 WHERE id = ?1");
            let row = connection
                .query_row(&sql, [&id], |row| Ok(node_row(row)))
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Lists the direct children of a session, oldest first.
    pub async fn children(&self, id: &str) -> Result<Vec<SessionNodeRow>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {NODE_COLUMNS} FROM session_v2
                 WHERE parent_id = ?1 ORDER BY time_created ASC, id ASC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([&id])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(node_row(row)?);
            }
            Ok(items)
        })
        .await
    }

    /// Adjusts a session's `child_count`, clamped at zero, in one statement.
    pub async fn increment_child_count(&self, id: &str, delta: i32) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE session_v2 SET child_count = max(0, child_count + ?2) WHERE id = ?1",
                    params![&id, i64::from(delta)],
                )?;
                if changed == 0 {
                    return Err(StoreError::NotFound(format!("session {id}")));
                }
                Ok(())
            })
        })
        .await
    }

    /// Inserts a new active goal. An active goal already registered for the
    /// root is a `Conflict`.
    pub async fn set_goal(&self, new: NewGoal) -> Result<Value, StoreError> {
        if new.objective.trim().is_empty() {
            return Err(StoreError::Invalid(
                "goal objective must not be empty".to_owned(),
            ));
        }
        let criteria = encode_list(&new.criteria)?;
        let NewGoal {
            id,
            root_id,
            objective,
            model,
            time_created,
            ..
        } = new;
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let active: Option<i64> = transaction
                    .query_row(
                        "SELECT 1 FROM goal WHERE root_id = ?1 AND status = 'active'",
                        [&root_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if active.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "root {root_id} already has an active goal"
                    )));
                }
                let duplicate: Option<i64> = transaction
                    .query_row("SELECT 1 FROM goal WHERE id = ?1", [&id], |row| row.get(0))
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!("goal {id} already exists")));
                }
                transaction
                    .execute(
                        "INSERT INTO goal
                            (id, root_id, objective, criteria, status, rounds, stalls,
                             evidence, model, time_created, time_updated)
                         VALUES (?1, ?2, ?3, ?4, 'active', 0, 0, '[]', ?5, ?6, ?6)",
                        params![&id, &root_id, &objective, &criteria, &model, time_created],
                    )
                    .map_err(map_goal_conflict)?;
                let sql = format!("SELECT {GOAL_COLUMNS} FROM goal WHERE id = ?1");
                let row = transaction
                    .query_row(&sql, [&id], |row| Ok(goal_value(row)))
                    .optional()?;
                row.transpose()?
                    .ok_or_else(|| StoreError::NotFound(format!("goal {id}")))
            })
        })
        .await
    }

    /// Loads the current goal for a root: the latest `active` goal, then the
    /// latest `paused`/`blocked` one, otherwise the latest goal of any status.
    pub async fn get_goal(&self, root_id: &str) -> Result<Option<Value>, StoreError> {
        let root_id = root_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {GOAL_COLUMNS} FROM goal WHERE root_id = ?1
                 ORDER BY CASE status
                              WHEN 'active' THEN 0
                              WHEN 'paused' THEN 1
                              WHEN 'blocked' THEN 2
                              ELSE 3
                          END,
                          time_updated DESC, id DESC
                 LIMIT 1"
            );
            let row = connection
                .query_row(&sql, [&root_id], |row| Ok(goal_value(row)))
                .optional()?;
            row.transpose()
        })
        .await
    }

    /// Applies a patch to a goal. Unknown ids yield `None`; moving a goal to
    /// `active` while another active goal exists for the root is a `Conflict`.
    pub async fn update_goal(
        &self,
        id: &str,
        patch: GoalPatch,
    ) -> Result<Option<Value>, StoreError> {
        if let Some(status) = &patch.status
            && status.trim().is_empty()
        {
            return Err(StoreError::Invalid(
                "goal status must not be empty".to_owned(),
            ));
        }
        let id = id.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let mut assignments: Vec<&'static str> = Vec::new();
                let mut values: Vec<SqlValue> = Vec::new();
                if let Some(status) = &patch.status {
                    assignments.push("status = ?");
                    values.push(SqlValue::Text(status.clone()));
                }
                if let Some(rounds) = patch.rounds {
                    assignments.push("rounds = ?");
                    values.push(SqlValue::Integer(i64::from(rounds)));
                }
                if let Some(stalls) = patch.stalls {
                    assignments.push("stalls = ?");
                    values.push(SqlValue::Integer(i64::from(stalls)));
                }
                if let Some(last_progress) = patch.last_progress {
                    assignments.push("last_progress = ?");
                    values.push(SqlValue::Integer(last_progress));
                }
                if let Some(evidence) = &patch.evidence {
                    assignments.push("evidence = ?");
                    values.push(SqlValue::Text(encode_list(evidence)?));
                }
                assignments.push("time_updated = ?");
                values.push(SqlValue::Integer(time));
                values.push(SqlValue::Text(id.clone()));
                let sql = format!("UPDATE goal SET {} WHERE id = ?", assignments.join(", "));
                let changed = transaction
                    .execute(&sql, params_from_iter(values.iter()))
                    .map_err(map_goal_conflict)?;
                if changed == 0 {
                    return Ok(None);
                }
                let read = format!("SELECT {GOAL_COLUMNS} FROM goal WHERE id = ?1");
                let row = transaction
                    .query_row(&read, [&id], |row| Ok(goal_value(row)))
                    .optional()?;
                row.transpose()
            })
        })
        .await
    }

    /// Lists every goal recorded for a root, newest first.
    pub async fn list_goals(&self, root_id: &str) -> Result<Vec<Value>, StoreError> {
        let root_id = root_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {GOAL_COLUMNS} FROM goal WHERE root_id = ?1
                 ORDER BY time_created DESC, id DESC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query([&root_id])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(goal_value(row)?);
            }
            Ok(items)
        })
        .await
    }

    /// Records an unsettled observation strip in state `running`.
    pub async fn record_observation(&self, new: NewObservation) -> Result<(), StoreError> {
        if new.tool.trim().is_empty() {
            return Err(StoreError::Invalid(
                "observation tool must not be empty".to_owned(),
            ));
        }
        let NewObservation {
            id,
            root_id,
            session_id,
            tool_call_id,
            tool,
            why,
            expect_s,
            intervene_s,
            started,
            parent_call,
            synthetic,
        } = new;
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let duplicate: Option<i64> = transaction
                    .query_row("SELECT 1 FROM observation WHERE id = ?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!(
                        "observation {id} already exists"
                    )));
                }
                transaction.execute(
                    "INSERT INTO observation
                        (id, root_id, session_id, tool_call_id, tool, why, expect_s,
                         intervene_s, started, parent_call, synthetic, state)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'running')",
                    params![
                        &id,
                        &root_id,
                        &session_id,
                        &tool_call_id,
                        &tool,
                        &why,
                        i64::from(expect_s),
                        i64::from(intervene_s),
                        started,
                        &parent_call,
                        i64::from(synthetic),
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    /// Settles an observation once. Returns `false` for unknown or
    /// already-settled ids.
    pub async fn settle_observation(
        &self,
        id: &str,
        state: &str,
        outcome: Option<&str>,
        settled: i64,
    ) -> Result<bool, StoreError> {
        if state.trim().is_empty() {
            return Err(StoreError::Invalid(
                "observation state must not be empty".to_owned(),
            ));
        }
        let id = id.to_owned();
        let state = state.to_owned();
        let outcome = outcome.map(str::to_owned);
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE observation SET state = ?2, outcome = ?3, settled = ?4
                     WHERE id = ?1 AND settled IS NULL",
                    params![&id, &state, &outcome, settled],
                )?;
                Ok(changed > 0)
            })
        })
        .await
    }

    /// Lists observation strips for a root, newest first.
    ///
    /// Additive to `docs/19`: the observer and UI need to read strips back, and
    /// it makes the documented wire shape reachable.
    pub async fn list_observations(
        &self,
        root_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>, StoreError> {
        let limit = if limit == 0 { 50 } else { limit.min(500) };
        let root_id = root_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {OBSERVATION_COLUMNS} FROM observation WHERE root_id = ?1
                 ORDER BY started DESC, id DESC LIMIT ?2"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows =
                statement.query(params![&root_id, i64::try_from(limit).unwrap_or(i64::MAX)])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(observation_value(row)?);
            }
            Ok(items)
        })
        .await
    }

    /// Records one observer repair attempt.
    pub async fn record_repair(&self, new: NewRepair) -> Result<(), StoreError> {
        if new.kind.trim().is_empty() {
            return Err(StoreError::Invalid(
                "repair kind must not be empty".to_owned(),
            ));
        }
        let NewRepair {
            id,
            root_id,
            target_id,
            kind,
            reason,
            args_hash,
            attempt,
            time_created,
        } = new;
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let duplicate: Option<i64> = transaction
                    .query_row(
                        "SELECT 1 FROM observer_repair WHERE id = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if duplicate.is_some() {
                    return Err(StoreError::Conflict(format!("repair {id} already exists")));
                }
                transaction.execute(
                    "INSERT INTO observer_repair
                        (id, root_id, target_id, kind, reason, args_hash, attempt, time_created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        &id,
                        &root_id,
                        &target_id,
                        &kind,
                        &reason,
                        &args_hash,
                        i64::from(attempt),
                        time_created,
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    /// Lists observer repairs for a root, newest first.
    pub async fn list_repairs(
        &self,
        root_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>, StoreError> {
        let limit = if limit == 0 { 50 } else { limit.min(500) };
        let root_id = root_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {REPAIR_COLUMNS} FROM observer_repair WHERE root_id = ?1
                 ORDER BY time_created DESC, id DESC LIMIT ?2"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows =
                statement.query(params![&root_id, i64::try_from(limit).unwrap_or(i64::MAX)])?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(repair_value(row)?);
            }
            Ok(items)
        })
        .await
    }
}

fn node_row(row: &Row<'_>) -> Result<SessionNodeRow, StoreError> {
    let id: String = row.get(0)?;
    let parent_id: Option<String> = row.get(1)?;
    let root_id: Option<String> = row.get(2)?;
    let depth: i64 = row.get(3)?;
    let role: Option<String> = row.get(4)?;
    let spawn_allowed: i64 = row.get(5)?;
    let child_count: i64 = row.get(6)?;
    let agent: Option<String> = row.get(7)?;
    let model: Option<String> = row.get(8)?;
    let busy: Option<i64> = row.get(9)?;
    let idle: Option<i64> = row.get(10)?;
    let outcome: Option<String> = row.get(11)?;
    let archived: Option<i64> = row.get(12)?;
    let suspended: Option<i64> = row.get(13)?;
    let idle_reason: Option<String> = row.get(14)?;
    Ok(SessionNodeRow {
        status: node_status(archived, busy, idle, outcome.as_deref(), suspended),
        root_id: root_id.unwrap_or_else(|| id.clone()),
        id,
        parent_id,
        depth: u32::try_from(depth).unwrap_or_default(),
        role: role.unwrap_or_else(|| "primary".to_owned()),
        spawn_allowed: spawn_allowed != 0,
        child_count: u32::try_from(child_count).unwrap_or_default(),
        agent: agent.unwrap_or_else(|| "build".to_owned()),
        model: model.and_then(|raw| compact_model(&raw)),
        idle_reason: idle_reason
            .filter(|reason| crate::session::keeps_idle_reason(outcome.as_deref(), reason)),
    })
}

fn node_status(
    archived: Option<i64>,
    busy: Option<i64>,
    idle: Option<i64>,
    outcome: Option<&str>,
    suspended: Option<i64>,
) -> String {
    if archived.is_some() {
        return "archived".to_owned();
    }
    if let Some(busy) = busy
        && idle.is_none_or(|idle| busy > idle)
    {
        return "busy".to_owned();
    }
    if suspended.is_some() {
        return "suspended".to_owned();
    }
    match outcome {
        Some("failed") => "failed",
        Some("interrupted") => "interrupted",
        Some("succeeded" | "completed" | "step_limit") => "completed",
        _ => "idle",
    }
    .to_owned()
}

fn compact_model(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let provider = value.get("providerID")?.as_str()?;
    let id = value.get("id")?.as_str()?;
    match value
        .get("variant")
        .and_then(Value::as_str)
        .filter(|variant| !variant.is_empty())
    {
        Some(variant) => Some(format!("{provider}/{id}#{variant}")),
        None => Some(format!("{provider}/{id}")),
    }
}

fn goal_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let root_id: String = row.get(1)?;
    let objective: String = row.get(2)?;
    let criteria: Option<String> = row.get(3)?;
    let status: String = row.get(4)?;
    let rounds: Option<i64> = row.get(5)?;
    let stalls: Option<i64> = row.get(6)?;
    let last_progress: Option<i64> = row.get(7)?;
    let evidence: Option<String> = row.get(8)?;
    let model: Option<String> = row.get(9)?;
    let time_created: Option<i64> = row.get(10)?;
    let time_updated: Option<i64> = row.get(11)?;

    // `done`/`total` are derived from `criteria` and `status`, never stored:
    // evidence ids are opaque handles with no per-criterion attribution, so a
    // goal counts as fully done only once its row is accepted `done` (the same
    // rule as `oc_runtime::goal_store::snapshot`). They are always integers.
    let criteria = string_array(criteria.as_deref(), "goal criteria")?;
    let total = u32::try_from(criteria.as_array().map_or(0, Vec::len)).unwrap_or(u32::MAX);
    let done = if status == "done" { total } else { 0 };

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("rootID".to_owned(), Value::String(root_id));
    map.insert("objective".to_owned(), Value::String(objective));
    map.insert("criteria".to_owned(), criteria);
    map.insert("done".to_owned(), json!(done));
    map.insert("total".to_owned(), json!(total));
    map.insert("status".to_owned(), Value::String(status));
    map.insert("rounds".to_owned(), json!(rounds.unwrap_or(0)));
    map.insert("stalls".to_owned(), json!(stalls.unwrap_or(0)));
    if let Some(last_progress) = last_progress {
        map.insert("lastProgress".to_owned(), json!(last_progress));
    }
    map.insert(
        "evidence".to_owned(),
        string_array(evidence.as_deref(), "goal evidence")?,
    );
    if let Some(model) = model {
        map.insert("model".to_owned(), Value::String(model));
    }
    map.insert(
        "time".to_owned(),
        json!({
            "created": time_created.unwrap_or(0),
            "updated": time_updated.unwrap_or(0),
        }),
    );
    Ok(Value::Object(map))
}

fn observation_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let root_id: Option<String> = row.get(1)?;
    let session_id: Option<String> = row.get(2)?;
    let tool_call_id: Option<String> = row.get(3)?;
    let tool: Option<String> = row.get(4)?;
    let why: Option<String> = row.get(5)?;
    let expect_s: Option<i64> = row.get(6)?;
    let intervene_s: Option<i64> = row.get(7)?;
    let started: Option<i64> = row.get(8)?;
    let settled: Option<i64> = row.get(9)?;
    let state: Option<String> = row.get(10)?;
    let outcome: Option<String> = row.get(11)?;
    let parent_call: Option<String> = row.get(12)?;
    let synthetic: Option<i64> = row.get(13)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert(
        "rootID".to_owned(),
        Value::String(root_id.unwrap_or_default()),
    );
    map.insert(
        "sessionID".to_owned(),
        Value::String(session_id.unwrap_or_default()),
    );
    map.insert(
        "toolCallID".to_owned(),
        Value::String(tool_call_id.unwrap_or_default()),
    );
    map.insert("tool".to_owned(), Value::String(tool.unwrap_or_default()));
    map.insert("why".to_owned(), Value::String(why.unwrap_or_default()));
    map.insert(
        "expectSeconds".to_owned(),
        json!(u32::try_from(expect_s.unwrap_or(0)).unwrap_or_default()),
    );
    map.insert(
        "interveneSeconds".to_owned(),
        json!(u32::try_from(intervene_s.unwrap_or(0)).unwrap_or_default()),
    );
    map.insert("started".to_owned(), json!(started.unwrap_or(0)));
    if let Some(settled) = settled {
        map.insert("settled".to_owned(), json!(settled));
    }
    map.insert(
        "state".to_owned(),
        Value::String(state.unwrap_or_else(|| "running".to_owned())),
    );
    if let Some(outcome) = outcome {
        map.insert("outcome".to_owned(), Value::String(outcome));
    }
    if let Some(parent_call) = parent_call {
        map.insert("parentCall".to_owned(), Value::String(parent_call));
    }
    map.insert(
        "synthetic".to_owned(),
        Value::Bool(synthetic.unwrap_or(0) != 0),
    );
    Ok(Value::Object(map))
}

fn repair_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let root_id: Option<String> = row.get(1)?;
    let target_id: Option<String> = row.get(2)?;
    let kind: Option<String> = row.get(3)?;
    let reason: Option<String> = row.get(4)?;
    let args_hash: Option<String> = row.get(5)?;
    let attempt: Option<i64> = row.get(6)?;
    let time_created: Option<i64> = row.get(7)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert(
        "rootID".to_owned(),
        Value::String(root_id.unwrap_or_default()),
    );
    map.insert(
        "targetID".to_owned(),
        Value::String(target_id.unwrap_or_default()),
    );
    map.insert("kind".to_owned(), Value::String(kind.unwrap_or_default()));
    map.insert(
        "reason".to_owned(),
        Value::String(reason.unwrap_or_default()),
    );
    map.insert(
        "argsHash".to_owned(),
        Value::String(args_hash.unwrap_or_default()),
    );
    map.insert("attempt".to_owned(), json!(attempt.unwrap_or(0)));
    map.insert(
        "time".to_owned(),
        json!({ "created": time_created.unwrap_or(0) }),
    );
    Ok(Value::Object(map))
}

fn encode_list(items: &[String]) -> Result<String, StoreError> {
    Ok(serde_json::to_string(items)?)
}

fn string_array(raw: Option<&str>, label: &str) -> Result<Value, StoreError> {
    let Some(raw) = raw else {
        return Ok(Value::Array(Vec::new()));
    };
    let items: Vec<String> = serde_json::from_str(raw)
        .map_err(|error| StoreError::Corrupt(format!("{label} are invalid: {error}")))?;
    Ok(Value::Array(items.into_iter().map(Value::String).collect()))
}

/// Maps the partial unique index violation on `goal_active_root` to a typed
/// conflict while leaving other constraint failures as SQL errors.
fn map_goal_conflict(error: rusqlite::Error) -> StoreError {
    let text = error.to_string();
    if text.contains("goal_active_root") || text.contains("UNIQUE constraint failed: goal") {
        StoreError::Conflict("an active goal already exists for this root".to_owned())
    } else {
        StoreError::Sqlite(error)
    }
}
