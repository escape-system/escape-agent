//! `GET /api/experimental/session/stats`: full-scan usage aggregation.

mod aggregate;

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

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
    TokenCounts, date_key, day_ordinal, day_ordinal_from_key, float_at, median, parse_epoch,
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

struct ExtractedToolCall {
    name: String,
    status: String,
    duration: Option<u32>,
}

struct AssistantExtract {
    tokens: TokenCounts,
    cost: f64,
    model: Option<(String, String, String)>,
    tools: Vec<ExtractedToolCall>,
}

struct ScannedMessage {
    session_id: Arc<str>,
    is_subagent: bool,
    is_user: bool,
    time: i64,
    assistant: Option<AssistantExtract>,
}

fn extract_assistant(message: &Value, include_tools: bool) -> AssistantExtract {
    let model = message.get("model").and_then(|model| {
        let provider = model
            .get("providerID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = model.get("id").and_then(Value::as_str).unwrap_or_default();
        if provider.is_empty() || id.is_empty() {
            return None;
        }
        let variant = model
            .get("variant")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Some((provider.to_owned(), id.to_owned(), variant.to_owned()))
    });
    let tools =
        if include_tools && let Some(content) = message.get("content").and_then(Value::as_array) {
            content
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool"))
                .map(|part| {
                    let name = part
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned();
                    let status = part
                        .pointer("/state/status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let ran = part.pointer("/time/ran").and_then(Value::as_i64);
                    let completed = part.pointer("/time/completed").and_then(Value::as_i64);
                    let duration = match (ran, completed) {
                        (Some(ran), Some(completed)) if completed >= ran => {
                            u32::try_from(completed - ran).ok()
                        }
                        _ => None,
                    };
                    ExtractedToolCall {
                        name,
                        status,
                        duration,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

    AssistantExtract {
        tokens: TokenCounts::from_message(message),
        cost: float_at(message, "/cost"),
        model,
        tools,
    }
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
        let is_subagent = session.get("parentID").and_then(Value::as_str).is_some();
        let created = session
            .get("time")
            .and_then(|time| time.get("created"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let sid: Arc<str> = Arc::from(session_id.as_str());
        for message in messages_all(store, &session_id).await? {
            let kind = message
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
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
            let is_user = kind == "user";
            let assistant = if is_user {
                None
            } else {
                Some(extract_assistant(&message, tools_mode != "none"))
            };
            scanned.push(ScannedMessage {
                session_id: Arc::clone(&sid),
                is_subagent,
                is_user,
                time,
                assistant,
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

    let mut unique_sessions: HashSet<Arc<str>> = HashSet::new();
    let mut subagents: HashSet<Arc<str>> = HashSet::new();
    let mut prompts = 0_i64;
    let mut steps = 0_i64;
    let mut tokens = TokenCounts::default();
    let mut cost = 0.0_f64;
    let mut activity: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    let mut models: HashMap<String, (Value, i64, TokenCounts, f64)> = HashMap::new();
    let mut tools: HashMap<String, ToolAggregate> = HashMap::new();

    for message in &scanned {
        if message.time < from {
            continue;
        }
        if message.is_subagent {
            subagents.insert(Arc::clone(&message.session_id));
        } else {
            unique_sessions.insert(Arc::clone(&message.session_id));
        }
        if message.is_user {
            if !message.is_subagent {
                prompts += 1;
            }
            continue;
        }
        let Some(ref extracted) = message.assistant else {
            continue;
        };

        steps += 1;
        tokens.add_assign(extracted.tokens);
        cost += extracted.cost;

        let ordinal = day_ordinal(message.time);
        let entry = activity
            .entry(ordinal)
            .or_insert_with(|| (date_key(message.time), 0));
        entry.1 += 1;

        if let Some((provider, model_id, variant)) = &extracted.model {
            let key = format!("{provider}/{model_id}#{variant}");
            let entry = models.entry(key).or_insert_with(|| {
                let mut reference = Map::new();
                reference.insert("providerID".to_owned(), json!(provider));
                reference.insert("id".to_owned(), json!(model_id));
                if !variant.is_empty() {
                    reference.insert("variant".to_owned(), json!(variant));
                }
                (Value::Object(reference), 0, TokenCounts::default(), 0.0)
            });
            entry.1 += 1;
            entry.2.add_assign(extracted.tokens);
            entry.3 += extracted.cost;
        }

        if tools_mode != "none" {
            for part in &extracted.tools {
                let entry = tools.entry(part.name.clone()).or_default();
                entry.calls += 1;
                match part.status.as_str() {
                    "completed" => entry.succeeded += 1,
                    "error" => entry.failed += 1,
                    _ => entry.unfinished += 1,
                }
                if let Some(duration) = part.duration {
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
                tokens.total(),
                json!({
                    "model": model,
                    "steps": steps,
                    "tokens": tokens.to_json(),
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
            "tokens": tokens.to_json(),
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
