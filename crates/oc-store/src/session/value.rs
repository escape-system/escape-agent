//! Row-to-wire mapping for `session_v2` plus the SQL value helpers.

use rusqlite::Row;
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};

use crate::db::{self, Store};
use crate::error::StoreError;

impl Store {
    /// Returns the count of active (currently busy or running) children of `parent_id`.
    pub async fn active_child_count(&self, parent_id: &str) -> Result<u32, StoreError> {
        let parent_id = parent_id.to_owned();
        self.read(move |connection| {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) FROM session_v2 
                     WHERE parent_id = ?1 
                       AND time_archived IS NULL
                       AND idle_outcome IS NULL
                       AND (time_idle IS NULL OR (time_busy IS NOT NULL AND time_busy > time_idle))",
                [&parent_id],
                |row| row.get(0),
            )?;
            Ok(u32::try_from(count).unwrap_or(u32::MAX))
        })
        .await
    }
}

pub(super) fn session_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let project_id: String = row.get(1)?;
    let parent_id: Option<String> = row.get(2)?;
    let fork_session_id: Option<String> = row.get(8)?;
    let fork_boundary: Option<String> = row.get(9)?;
    let directory: Option<String> = row.get(10)?;
    let path: Option<String> = row.get(11)?;
    let title: Option<String> = row.get(12)?;
    let agent: Option<String> = row.get(13)?;
    let metadata: Option<String> = row.get(14)?;
    let permission: Option<String> = row.get(15)?;
    let model: Option<String> = row.get(16)?;
    let revert: Option<String> = row.get(17)?;
    let cost: f64 = row.get(19)?;
    let tokens_input: i64 = row.get(20)?;
    let tokens_output: i64 = row.get(21)?;
    let tokens_reasoning: i64 = row.get(22)?;
    let tokens_cache_read: i64 = row.get(23)?;
    let tokens_cache_write: i64 = row.get(24)?;
    let time_created: i64 = row.get(25)?;
    let time_updated: i64 = row.get(26)?;
    let time_busy: Option<i64> = row.get(27)?;
    let time_idle: Option<i64> = row.get(28)?;
    let time_viewed: Option<i64> = row.get(29)?;
    let idle_outcome: Option<String> = row.get(30)?;
    let time_archived: Option<i64> = row.get(32)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("projectID".to_owned(), Value::String(project_id));
    if let Some(title) = title {
        map.insert("title".to_owned(), Value::String(title));
    }
    if let Some(parent_id) = parent_id {
        map.insert("parentID".to_owned(), Value::String(parent_id));
    }
    if let (Some(session_id), Some(boundary)) = (fork_session_id, fork_boundary) {
        map.insert(
            "fork".to_owned(),
            json!({
                "sessionID": session_id,
                "boundary": db::decode_json(&boundary)?,
            }),
        );
    }
    if let Some(agent) = agent {
        map.insert("agent".to_owned(), Value::String(agent));
    }
    if let Some(model) = model {
        map.insert("model".to_owned(), db::decode_json(&model)?);
    }
    map.insert("cost".to_owned(), json!(cost));
    map.insert(
        "tokens".to_owned(),
        json!({
            "input": tokens_input,
            "output": tokens_output,
            "reasoning": tokens_reasoning,
            "cache": { "read": tokens_cache_read, "write": tokens_cache_write },
        }),
    );
    if let Some(directory) = directory {
        map.insert("location".to_owned(), json!({ "directory": directory }));
    }
    if let Some(path) = path {
        map.insert("subpath".to_owned(), Value::String(path));
    }
    if let Some(metadata) = metadata {
        map.insert("metadata".to_owned(), db::decode_json(&metadata)?);
    }
    if let Some(permission) = permission {
        map.insert("permissions".to_owned(), db::decode_json(&permission)?);
    }
    if let Some(revert) = revert {
        map.insert("revert".to_owned(), db::decode_json(&revert)?);
    }
    if let Some(outcome) = idle_outcome {
        map.insert("outcome".to_owned(), Value::String(outcome));
    }
    let mut time = Map::new();
    time.insert("created".to_owned(), json!(time_created));
    time.insert("updated".to_owned(), json!(time_updated));
    if let Some(busy) = time_busy {
        time.insert("busy".to_owned(), json!(busy));
    }
    if let Some(idle) = time_idle {
        time.insert("idle".to_owned(), json!(idle));
    }
    if let Some(viewed) = time_viewed {
        time.insert("viewed".to_owned(), json!(viewed));
    }
    if let Some(archived) = time_archived {
        time.insert("archived".to_owned(), json!(archived));
    }
    map.insert("time".to_owned(), Value::Object(time));
    Ok(Value::Object(map))
}

pub(super) fn json_sql(value: Option<&Value>) -> Result<SqlValue, StoreError> {
    match value {
        Some(Value::Null) | None => Ok(SqlValue::Null),
        Some(value) => Ok(SqlValue::Text(serde_json::to_string(value)?)),
    }
}
