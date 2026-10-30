//! `GET /api/experimental/session/stats`: full-scan usage aggregation.

mod aggregate;

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use oc_types::{ApiError, now_millis};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::{query_error, require_store};
use crate::error::ErrorResponse;

use self::aggregate::{
    add_tokens, date_key, day_ordinal, day_ordinal_from_key, empty_tokens, float_at, median,
    parse_epoch, tokens_total,
};
use super::common::{id_of, messages_all, sessions_all, time_of};

/// `GET /api/experimental/session/stats`
#[derive(Debug, Default, Deserialize)]
pub struct StatsParams {
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    tools: Option<String>,
}

struct ScannedMessage {
    session_id: String,
    parent_id: Option<String>,
    kind: String,
    time: i64,
    data: Value,
}

#[derive(Default)]
struct ToolAggregate {
    calls: i64,
    succeeded: i64,
    failed: i64,
    unfinished: i64,
    durations: Vec<u32>,
}

/// `GET /api/experimental/session/stats`
#[allow(clippy::too_many_lines)] // one full-scan aggregator; splitting hides the shared accumulators
pub async fn stats(
    State(state): State<AppState>,
    params: Result<Query<StatsParams>, QueryRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    if let Some(timezone) = params.timezone.as_deref()
        && !matches!(timezone, "UTC" | "Etc/UTC" | "utc")
    {
        // Only UTC is supported offline; refuse rather than bucket by the
        // wrong civil day.
        return Err(ApiError::new(
            400,
            "InvalidRequestError",
            format!("Unsupported timezone {timezone:?}; only UTC is available"),
        )
        .into());
    }
    let tools_mode = params.tools.as_deref().unwrap_or("summary");
    if !matches!(tools_mode, "none" | "summary" | "detail") {
        return Err(ApiError::new(
            400,
            "InvalidRequestError",
            format!("tools must be none, summary or detail, got {tools_mode:?}"),
        )
        .into());
    }

    let store = require_store(&state)?;
    let to = match params.to.as_deref() {
        Some(raw) => parse_epoch(raw, "to")?,
        None => now_millis(),
    };

    let sessions = sessions_all(store, params.project.as_deref()).await?;
    let mut scanned: Vec<ScannedMessage> = Vec::new();
    let mut earliest: Option<i64> = None;
    for session in &sessions {
        let session_id = id_of(session);
        let parent_id = session
            .get("parentID")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let created = session
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        for message in messages_all(store, &session_id).await? {
            let kind = message
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if kind != "user" && kind != "assistant" {
                continue;
            }
            let time = time_of(&message).unwrap_or_default();
            // Forked sessions carry a copy of the parent's history; only rows
            // recorded after the fork count.
            if time < created || time >= to {
                continue;
            }
            earliest = Some(earliest.map_or(time, |current| current.min(time)));
            scanned.push(ScannedMessage {
                session_id: session_id.clone(),
                parent_id: parent_id.clone(),
                kind,
                time,
                data: message,
            });
        }
    }

    let explicit_from = params.from.is_some();
    let from = match params.from.as_deref() {
        Some(raw) => parse_epoch(raw, "from")?,
        None => earliest.unwrap_or(to),
    };
    if explicit_from && from >= to {
        return Err(
            ApiError::new(400, "InvalidRequestError", "from must be earlier than to").into(),
        );
    }

    let mut unique_sessions = HashSet::new();
    let mut subagents = HashSet::new();
    let mut prompts = 0_i64;
    let mut steps = 0_i64;
    let mut tokens = empty_tokens();
    let mut cost = 0.0_f64;
    let mut activity: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    let mut models: HashMap<String, (Value, i64, Value, f64)> = HashMap::new();
    let mut tools: HashMap<String, ToolAggregate> = HashMap::new();

    for message in &scanned {
        if message.time < from {
            continue;
        }
        if message.parent_id.is_none() {
            unique_sessions.insert(message.session_id.clone());
        } else {
            subagents.insert(message.session_id.clone());
        }
        if message.kind == "user" {
            if message.parent_id.is_none() {
                prompts += 1;
            }
            continue;
        }

        steps += 1;
        add_tokens(&mut tokens, &message.data);
        cost += float_at(&message.data, "/cost");

        let ordinal = day_ordinal(message.time);
        let entry = activity
            .entry(ordinal)
            .or_insert_with(|| (date_key(message.time), 0));
        entry.1 += 1;

        let provider = message
            .data
            .pointer("/model/providerID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let model_id = message
            .data
            .pointer("/model/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !provider.is_empty() && !model_id.is_empty() {
            let variant = message
                .data
                .pointer("/model/variant")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = format!("{provider}/{model_id}#{variant}");
            let entry = models.entry(key).or_insert_with(|| {
                let mut reference = Map::new();
                reference.insert("providerID".to_owned(), json!(provider));
                reference.insert("id".to_owned(), json!(model_id));
                if !variant.is_empty() {
                    reference.insert("variant".to_owned(), json!(variant));
                }
                (Value::Object(reference), 0, empty_tokens(), 0.0)
            });
            entry.1 += 1;
            add_tokens(&mut entry.2, &message.data);
            entry.3 += float_at(&message.data, "/cost");
        }

        if tools_mode != "none"
            && let Some(content) = message.data.get("content").and_then(Value::as_array)
        {
            for part in content {
                if part.get("type").and_then(Value::as_str) != Some("tool") {
                    continue;
                }
                let name = part
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let status = part
                    .pointer("/state/status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let entry = tools.entry(name).or_default();
                entry.calls += 1;
                match status {
                    "completed" => entry.succeeded += 1,
                    "error" => entry.failed += 1,
                    _ => entry.unfinished += 1,
                }
                let ran = part.pointer("/time/ran").and_then(Value::as_i64);
                let completed = part.pointer("/time/completed").and_then(Value::as_i64);
                if let (Some(ran), Some(completed)) = (ran, completed)
                    && completed >= ran
                    && let Ok(duration) = u32::try_from(completed - ran)
                {
                    entry.durations.push(duration);
                }
            }
        }
    }

    let tool_totals = tools.values().fold(
        (0_i64, 0_i64, 0_i64, 0_i64),
        |(calls, succeeded, failed, unfinished), entry| {
            (
                calls + entry.calls,
                succeeded + entry.succeeded,
                failed + entry.failed,
                unfinished + entry.unfinished,
            )
        },
    );

    let tool_value = match tools_mode {
        "none" => json!({ "mode": "none" }),
        "detail" => {
            let mut usage: Vec<(String, &ToolAggregate)> = tools
                .iter()
                .map(|(name, entry)| (name.clone(), entry))
                .collect();
            usage.sort_by_key(|(_, entry)| Reverse(entry.calls));
            let usage: Vec<Value> = usage
                .into_iter()
                .map(|(name, entry)| {
                    let mut durations = entry.durations.clone();
                    json!({
                        "name": name,
                        "calls": entry.calls,
                        "succeeded": entry.succeeded,
                        "failed": entry.failed,
                        "unfinished": entry.unfinished,
                        "durationP50": median(&mut durations),
                    })
                })
                .collect();
            json!({
                "mode": "detail",
                "totals": {
                    "calls": tool_totals.0,
                    "succeeded": tool_totals.1,
                    "failed": tool_totals.2,
                    "unfinished": tool_totals.3,
                },
                "usage": usage,
            })
        }
        _ => json!({
            "mode": "summary",
            "totals": {
                "calls": tool_totals.0,
                "succeeded": tool_totals.1,
                "failed": tool_totals.2,
                "unfinished": tool_totals.3,
            },
        }),
    };

    let days: Vec<(String, i64)> = activity.into_values().collect();
    let streak = days
        .iter()
        .enumerate()
        .fold((0_i64, 0_i64), |(current, longest), (index, (date, _))| {
            let ordinal = day_ordinal_from_key(date);
            let previous = index
                .checked_sub(1)
                .and_then(|previous| days.get(previous))
                .map(|(date, _)| day_ordinal_from_key(date));
            let current = match previous {
                Some(previous) if ordinal - previous == 1 => current + 1,
                _ => 1,
            };
            (current, longest.max(current))
        })
        .1;
    let active_days = i64::try_from(days.len()).unwrap_or(i64::MAX);

    let mut models: Vec<(i64, Value)> = models
        .into_values()
        .map(|(model, steps, tokens, cost)| {
            (
                tokens_total(&tokens),
                json!({
                    "model": model,
                    "steps": steps,
                    "tokens": tokens,
                    "cost": cost,
                }),
            )
        })
        .collect();
    models.sort_by_key(|(total, _)| Reverse(*total));

    Ok(Json(json!({
        "data": {
            "range": { "from": from, "to": to },
            "sessions": unique_sessions.len(),
            "subagents": subagents.len(),
            "prompts": prompts,
            "steps": steps,
            "tokens": tokens,
            "cost": cost,
            "tools": tool_value,
            "activeDays": active_days,
            "streak": streak,
            "activity": days
                .into_iter()
                .map(|(date, steps)| json!({ "date": date, "steps": steps }))
                .collect::<Vec<_>>(),
            "models": models.into_iter().map(|(_, value)| value).collect::<Vec<_>>(),
        }
    })))
}
