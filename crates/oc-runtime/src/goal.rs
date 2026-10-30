//! Goal tools and the `oc/goal` prompt block (`docs/08` §6).
//!
//! A goal belongs to a root session; the store enforces one active goal per
//! root. The tools never decide completion themselves: `goal_complete` records
//! evidence, bumps the round counter and asks the observer to judge. Only the
//! observer may set `done`.
//!
//! `progress` and `blocked_reason` have no column in the goal schema, so
//! `goal_update` records them as a synthetic note while the durable machine
//! state lives in the goal row.

use std::fmt::Write as _;
use std::sync::Arc;

use oc_observer::GoalSnapshot;
use oc_store::{GoalPatch, NewGoal, NewMessage, StoreError};
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::{GoalId, MessageId, now_millis};
use serde_json::{Value, json};

use crate::runtime::Runtime;

/// Build the goal tool set for one runtime.
#[derive(Clone, Debug)]
pub struct GoalTools {
    runtime: Runtime,
}

impl GoalTools {
    #[must_use]
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }

    /// Registry entries: `goal_set`, `goal_update`, `goal_complete` and
    /// `goal_status`.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(GoalSetTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(GoalUpdateTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(GoalCompleteTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(GoalStatusTool {
            runtime: self.runtime.clone(),
        }));
        registry
    }
}

/// Render the cache-stable `<goal>` block shown to the model.
///
/// The block contains no clocks or session ids, so it changes only when the
/// goal row does. Criteria are completed in order: the snapshot's `done` count
/// corresponds to the first `done` evidence ids, matching
/// [`crate::StoreGoalStore`].
#[must_use]
pub fn render_goal(snapshot: &GoalSnapshot) -> String {
    let mut out = String::with_capacity(256);
    let _ = writeln!(
        out,
        "<goal id=\"{}\" status=\"{}\" round=\"{}\">",
        escape_attr(&snapshot.id),
        snapshot.status.as_str(),
        snapshot.rounds
    );
    let _ = writeln!(out, "Objective: {}", snapshot.objective);
    out.push_str("Criteria:\n");
    if snapshot.criteria.is_empty() {
        out.push_str("1. (none declared)\n");
    } else {
        let done = usize::try_from(snapshot.done).unwrap_or(usize::MAX);
        for (index, criterion) in snapshot.criteria.iter().enumerate() {
            let marker = if index < done {
                match snapshot.evidence.get(index) {
                    Some(evidence) => format!("[done: {evidence}]"),
                    None => "[done]".to_owned(),
                }
            } else {
                "[open]".to_owned()
            };
            let _ = writeln!(out, "{}. {} {marker}", index + 1, criterion);
        }
    }
    let next = snapshot
        .criteria
        .iter()
        .enumerate()
        .find(|(index, _)| *index >= usize::try_from(snapshot.done).unwrap_or(usize::MAX))
        .map(|(_, criterion)| criterion.as_str());
    let next = next.unwrap_or(if snapshot.status.is_terminal() {
        "none"
    } else {
        "awaiting the observer verdict"
    });
    let _ = writeln!(out, "Next: {next}");
    out.push_str("</goal>");
    out
}

struct GoalSetTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for GoalSetTool {
    fn name(&self) -> &'static str {
        "goal_set"
    }

    fn description(&self) -> &'static str {
        "Start a goal for this session tree. Fails when the root already has an active goal."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "One-line outcome the session tree must achieve."
                },
                "criteria": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Acceptance criteria the observer uses to judge completion."
                }
            },
            "required": ["objective", "criteria"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let objective = required_str(&input, "objective")?;
        let criteria = required_string_list(&input, "criteria")?;
        let node = self
            .runtime
            .inner
            .config
            .store
            .session_node(&ctx.session_id)
            .await
            .map_err(|error| store_error(&error))?
            .ok_or_else(|| {
                ToolError::new(format!("session {} is not registered", ctx.session_id))
            })?;
        let goal = self
            .runtime
            .inner
            .config
            .store
            .set_goal(NewGoal {
                id: GoalId::new().into_inner(),
                root_id: node.root_id.clone(),
                objective,
                criteria,
                model: node.model.clone(),
                time_created: now_millis(),
            })
            .await
            .map_err(|error| match error {
                StoreError::Conflict(message) => ToolError::new(format!(
                    "cannot set goal: {message}. Update or complete the existing goal first."
                )),
                other => store_error(&other),
            })?;
        Ok(ToolResult::text(format!(
            "Goal created for root session {}.\n{}",
            node.root_id,
            pretty(&goal)
        )))
    }
}

struct GoalUpdateTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for GoalUpdateTool {
    fn name(&self) -> &'static str {
        "goal_update"
    }

    fn description(&self) -> &'static str {
        "Report goal progress or block the goal. The observer still decides completion."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "progress": {
                    "type": "string",
                    "description": "What changed since the last report; refreshes last_progress."
                },
                "blocked_reason": {
                    "type": "string",
                    "description": "Why the goal cannot proceed; sets the goal status to blocked."
                }
            }
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let progress = optional_str(&input, "progress");
        let blocked = optional_str(&input, "blocked_reason");
        if progress.is_none() && blocked.is_none() {
            return Err(ToolError::new(
                "goal_update needs `progress` or `blocked_reason`",
            ));
        }

        let root = session_root(&self.runtime, &ctx.session_id).await?;
        let goal = load_goal(&self.runtime, &root).await?;
        let status = goal
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("active");
        if is_terminal(status) {
            return Err(ToolError::new(format!(
                "goal is already {status}; start a new goal with goal_set"
            )));
        }
        let id = goal
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("stored goal has no id"))?
            .to_owned();

        let patch = GoalPatch {
            last_progress: progress.as_ref().map(|_| now_millis()),
            status: blocked.as_ref().map(|_| "blocked".to_owned()),
            ..GoalPatch::default()
        };
        let mut notes = Vec::new();
        if let Some(progress) = &progress {
            notes.push(format!("Goal progress: {progress}"));
        }
        if let Some(reason) = &blocked {
            notes.push(format!("Goal blocked: {reason}"));
        }

        let updated = self
            .runtime
            .inner
            .config
            .store
            .update_goal(&id, patch)
            .await
            .map_err(|error| store_error(&error))?
            .ok_or_else(|| ToolError::new(format!("goal {id} disappeared")))?;

        if !notes.is_empty()
            && let Err(error) = record_note(&self.runtime, &ctx.session_id, &id, &notes).await
        {
            return Err(ToolError::new(format!(
                "goal was updated but the note could not be recorded: {error}"
            )));
        }

        Ok(ToolResult::text(pretty(&updated)))
    }
}

struct GoalCompleteTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for GoalCompleteTool {
    fn name(&self) -> &'static str {
        "goal_complete"
    }

    fn description(&self) -> &'static str {
        "Record evidence that the goal is complete and ask the observer to judge. Never marks the goal done directly."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "evidence": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Message or tool-call ids that satisfy the criteria."
                }
            },
            "required": ["evidence"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let evidence = required_string_list(&input, "evidence")?;
        let root = session_root(&self.runtime, &ctx.session_id).await?;
        let goal = load_goal(&self.runtime, &root).await?;
        let status = goal
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("active");
        if is_terminal(status) {
            return Err(ToolError::new(format!("goal is already {status}")));
        }
        let id = goal
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("stored goal has no id"))?
            .to_owned();
        let rounds = goal
            .get("rounds")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let rounds = u32::try_from(rounds).unwrap_or(u32::MAX);

        let mut merged = string_list(goal.get("evidence"));
        for item in evidence {
            if !merged.iter().any(|existing| existing == &item) {
                merged.push(item);
            }
        }

        let updated = self
            .runtime
            .inner
            .config
            .store
            .update_goal(
                &id,
                GoalPatch {
                    rounds: Some(rounds),
                    evidence: Some(merged),
                    ..GoalPatch::default()
                },
            )
            .await
            .map_err(|error| store_error(&error))?
            .ok_or_else(|| ToolError::new(format!("goal {id} disappeared")))?;

        self.runtime
            .request_goal_check(&root)
            .await
            .map_err(|error| {
                ToolError::new(format!(
                    "evidence was recorded but the observer check could not be requested: {error}"
                ))
            })?;

        Ok(ToolResult::text(format!(
            "Evidence recorded; the observer will decide whether the goal is done.\n{}",
            pretty(&updated)
        )))
    }
}

struct GoalStatusTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for GoalStatusTool {
    fn name(&self) -> &'static str {
        "goal_status"
    }

    fn description(&self) -> &'static str {
        "Return the goal for this session tree: status, rounds, stalls and evidence."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        let root = session_root(&self.runtime, &ctx.session_id).await?;
        let Some(goal) = self
            .runtime
            .inner
            .config
            .store
            .get_goal(&root)
            .await
            .map_err(|error| store_error(&error))?
        else {
            return Ok(ToolResult::text(format!(
                "No goal is set for root session {root}."
            )));
        };
        Ok(ToolResult::text(pretty(&goal)))
    }
}

async fn session_root(runtime: &Runtime, session: &str) -> Result<String, ToolError> {
    runtime
        .inner
        .config
        .store
        .session_node(session)
        .await
        .map_err(|error| store_error(&error))?
        .map(|node| node.root_id)
        .ok_or_else(|| ToolError::new(format!("session {session} is not registered")))
}

async fn load_goal(runtime: &Runtime, root: &str) -> Result<Value, ToolError> {
    runtime
        .inner
        .config
        .store
        .get_goal(root)
        .await
        .map_err(|error| store_error(&error))?
        .ok_or_else(|| ToolError::new(format!("no goal is set for root session {root}")))
}

/// Goal notes are synthetic messages: the goal row has no free-text column,
/// and messages keep the note in the transcript the model already reads.
async fn record_note(
    runtime: &Runtime,
    session: &str,
    goal: &str,
    notes: &[String],
) -> Result<(), StoreError> {
    let created = now_millis();
    runtime
        .inner
        .config
        .store
        .append_message(NewMessage {
            id: MessageId::new().into_inner(),
            session_id: session.to_owned(),
            kind: "synthetic".to_owned(),
            data: json!({
                "text": notes.join("\n"),
                "description": "goal update",
                "metadata": { "goalID": goal, "source": "goal" }
            }),
            time_created: created,
        })
        .await?;
    Ok(())
}

fn store_error(error: &StoreError) -> ToolError {
    ToolError::new(error.to_string())
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "failed" | "cancelled")
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("`{key}` is required and must be non-empty")))
}

fn optional_str(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn required_string_list(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    let values = input
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new(format!("`{key}` is required and must be an array")))?;
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let item = value
            .as_str()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .ok_or_else(|| ToolError::new(format!("`{key}` entries must be non-empty strings")))?;
        out.push(item.to_owned());
    }
    Ok(out)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use oc_observer::{GoalPolicy, GoalStatus};

    use super::*;

    fn snapshot() -> GoalSnapshot {
        GoalSnapshot {
            id: "gol_test".to_owned(),
            objective: "Ship it".to_owned(),
            criteria: vec!["build passes".to_owned(), "docs updated".to_owned()],
            done: 1,
            total: 2,
            status: GoalStatus::Active,
            rounds: 3,
            evidence: vec!["msg_one".to_owned()],
            policy: GoalPolicy::default(),
            ..GoalSnapshot::default()
        }
    }

    #[test]
    fn renders_the_documented_block() {
        assert_eq!(
            render_goal(&snapshot()),
            "<goal id=\"gol_test\" status=\"active\" round=\"3\">\n\
             Objective: Ship it\n\
             Criteria:\n\
             1. build passes [done: msg_one]\n\
             2. docs updated [open]\n\
             Next: docs updated\n\
             </goal>"
        );
    }

    #[test]
    fn escaping_keeps_the_attribute_valid() {
        let mut goal = snapshot();
        goal.id = "gol_\"<x>".to_owned();
        assert!(render_goal(&goal).starts_with("<goal id=\"gol_&quot;&lt;x&gt;\""));
    }
}
