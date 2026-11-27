use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::settings::{SubagentSettings, available_agents};
use super::tree::{optional_bool, optional_str, required_str};
use super::wait::{finish_foreground, spawn_watcher};

mod continue_child;
mod partition;
mod prepare;

use continue_child::continue_child;
use partition::execute_partitioned;
use prepare::create_and_notify_child;
use prepare::parse_optional_usize;
use prepare::prepare_spawn_context;
use prepare::reserve_spawn_slots;
use prepare::resolve_manual_partition;

#[derive(Debug, Clone)]
pub(super) struct SubagentTool {
    pub(super) runtime: Runtime,
    /// `subagents.background_default` captured when the tool was built, so the
    /// schema states the default the execution path will actually use.
    pub(super) background_default: bool,
}

impl SubagentTool {
    /// Schema text for `background`, naming the configured default and its
    /// consequence.
    fn background_description(&self) -> String {
        if self.background_default {
            "Defaults to true (subagents.background_default): returns immediately with \
             {\"status\":\"running\"} and the child's final text arrives later as a synthetic \
             parent message. Pass false to wait for the result in this call."
                .to_owned()
        } else {
            "Defaults to false (subagents.background_default): waits for the child's final text \
             in this call. Pass true to return immediately and receive the result later as a \
             synthetic parent message."
                .to_owned()
        }
    }

    /// Schema text naming the agents the model may spawn. Read per request so
    /// agents added to the configuration appear on the next turn.
    fn agent_description(&self) -> String {
        let config = &self.runtime.inner.config.config;
        let default_agent = config
            .merged()
            .pointer("/subagents/default_agent")
            .and_then(Value::as_str)
            .unwrap_or("general");
        let available = available_agents(config);
        if available.is_empty() {
            format!(
                "Subagent id to spawn. Omit it to use the configured default `{default_agent}`."
            )
        } else {
            format!(
                "Subagent id to spawn, one of: {}. Omit it to use the configured default \
                 `{default_agent}`.",
                available.join(", ")
            )
        }
    }
}

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &'static str {
        "Create a child session for a subagent, or continue an existing child. Call this before your own exploration when the user asks you to spawn subagents. Depending on `background` (default from configuration) it waits for the final text or returns immediately and delivers the result as a synthetic message. Use it to delegate self-contained work that can run in parallel; `agent` may be omitted to use the configured default. The parent sees only the final text; use digest for transcripts."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "description": self.agent_description() },
                "description": { "type": "string", "description": "Short label shown in the session tree." },
                "prompt": { "type": "string", "description": "Task text delivered to the child." },
                "model": { "type": "string", "description": "Optional model override: provider/id or provider/id#variant. Omit to inherit the agent or parent model." },
                "key": { "type": "string", "description": "Optional credential id, exact/normalized label, 'env', or 'auto' for balanced healthy-key rotation. Omit to automatically load-balance across multiple healthy keys or keep a healthy pinned parent key." },
                "sessionID": { "type": "string", "description": "Continue an existing direct child or descendant. Omit when spawning a new child." },
                "background": { "type": "boolean", "description": self.background_description() },
                "role": { "type": "string", "enum": ["worker", "coordinator"], "description": "Child policy role." },
                "spawn": {
                    "type": "object",
                    "properties": {
                        "allow": { "type": "boolean", "description": "Allow the child to spawn its own children." },
                        "max_depth": { "type": "integer", "minimum": 0, "description": "Absolute tree depth cap for this subtree (must be >= this child's depth, i.e. parent.depth + 1). Omit to use the configured default." },
                        "attach": { "type": "string", "enum": ["parent", "child"], "description": "Optional placement: 'parent' attaches the spawned worker directly under this coordinator's parent session (routing completions/talk back to the coordinator); 'child' nests beneath the caller. Coordinators with a parent session default to 'parent'." }
                    },
                    "additionalProperties": false
                },
                "lane": { "type": "string", "description": "Lane hint recorded for the lane service." },
                "group": { "type": "string", "description": "Optional group label recorded in session metadata (docs/21-grouping.md)." },
                "partitions": {
                    "type": "integer",
                    "minimum": 2,
                    "maximum": 16,
                    "description": "Batch mode: automatically partition the task via `code_map` graph clustering into N disjoint slices and launch N parallel workers in one call. Do not pass `partition_index` unless launching a single slice instead."
                },
                "partition_index": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 15,
                    "description": "Single-worker manual slice: 0-based slice index (pair with `partition_count`; omit `partitions` for clarity)."
                },
                "partition_count": {
                    "type": "integer",
                    "minimum": 2,
                    "maximum": 16,
                    "description": "Single-worker manual slice: total slice count paired with `partition_index` (omit when using `partitions`)."
                }
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let warm = runtime.load_warm_caller(&caller).await?;
        let settings = warm.settings.clone();
        let agent =
            optional_str(&input, "agent").unwrap_or_else(|| settings.limits.default_agent.clone());
        let description = required_str(&input, "description")?;
        let prompt = required_str(&input, "prompt")?;
        let background = optional_bool(&input, "background").unwrap_or(settings.background_default);
        let partitions = parse_optional_usize(&input, "partitions")?;
        let partition_index = parse_optional_usize(&input, "partition_index")?;
        let partition_count = parse_optional_usize(&input, "partition_count")?;

        if let Some(target) = optional_str(&input, "sessionID")
            .or_else(|| optional_str(&input, "session_id"))
            .filter(|s| {
                !matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "" | "none" | "null" | "undefined" | "new"
                )
            })
        {
            if partitions.is_some() || partition_index.is_some() || partition_count.is_some() {
                return Err(ToolError::new(
                    "partition parameters cannot be combined with sessionID",
                ));
            }
            return continue_child(
                runtime,
                &caller,
                &target,
                &prompt,
                &description,
                background,
                &settings,
                ctx.cancel.as_ref(),
            )
            .await;
        }

        let plan = normalize_partition_args(partitions, partition_index, partition_count)?;
        let (partitions, manual_slice) = match plan {
            PartitionPlan::None => (None, None),
            PartitionPlan::Auto(count) => (Some(count), None),
            PartitionPlan::Manual { index, count } => (
                None,
                resolve_manual_partition(ctx, &prompt, Some(index), Some(count)).await?,
            ),
        };

        let ctx_spawn = prepare_spawn_context(runtime, &caller, &agent, &input, &warm).await?;

        if let Some(count) = partitions {
            return execute_partitioned(ctx, &ctx_spawn, &description, &prompt, count, background)
                .await;
        }

        reserve_spawn_slots(
            runtime,
            &ctx_spawn.parent_id,
            &ctx_spawn.limits,
            ctx_spawn.active_children,
            1,
            ctx.cancel.as_ref(),
        )
        .await?;

        let (effective_prompt, partition_meta) = match manual_slice {
            Some((section, meta)) => (format!("{prompt}\n\n{section}"), Some(meta)),
            None => (prompt, None),
        };
        let child = create_and_notify_child(
            &ctx_spawn,
            &description,
            &effective_prompt,
            partition_meta.as_ref(),
        )
        .await?;
        let report_target = ctx_spawn.reports_to().to_owned();

        if background {
            spawn_watcher(
                runtime.clone(),
                report_target,
                child.session_id.clone(),
                agent,
                description,
                child.prompt_id,
                settings.timeout,
            );
            let payload = ctx_spawn.result_metadata(&child.session_id, "running");
            return Ok(ToolResult::text(payload.to_string()).with_metadata(payload));
        }
        let mut running_meta = serde_json::Map::new();
        running_meta.insert(
            "sessionID".to_owned(),
            Value::String(child.session_id.clone()),
        );
        running_meta.insert("status".to_owned(), Value::String("running".to_owned()));
        crate::turn::report_tool_progress(running_meta);
        let mut result = finish_foreground(
            runtime,
            &report_target,
            &child.session_id,
            &agent,
            &description,
            &child.prompt_id,
            &settings,
            ctx.cancel.as_ref(),
        )
        .await?;
        // The model-visible `<subagent …>` envelope is unchanged; the resolved
        // model, its provenance and the quota snapshot are metadata only, so
        // prompt caching and the documented parent view survive.
        ctx_spawn.annotate_metadata(&mut result);
        Ok(result)
    }
}

impl SpawnContext {
    /// `{sessionID, status}` plus the resolved-model report.
    fn result_metadata(&self, child: &str, status: &str) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("sessionID".to_owned(), Value::String(child.to_owned()));
        map.insert("status".to_owned(), Value::String(status.to_owned()));
        map.extend(self.report_metadata());
        Value::Object(map)
    }

    /// The optional report fields, omitted when unknown (never `null`).
    fn report_metadata(&self) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("model".to_owned(), self.model_value.clone());
        map.insert(
            "modelSource".to_owned(),
            Value::String(self.model_source.as_str().to_owned()),
        );
        if let Some(quota) = &self.quota_snapshot {
            map.insert("quota".to_owned(), quota.to_json());
        }
        // Only the cooling keys are reported; an all-healthy pool omits the field
        // entirely instead of publishing an empty array.
        if !self.key_health.is_empty() {
            map.insert(
                "keyHealth".to_owned(),
                Value::Array(self.key_health.clone()),
            );
        }
        if !self.rejected_models.is_empty() {
            map.insert(
                "rejectedModels".to_owned(),
                Value::Array(
                    self.rejected_models
                        .iter()
                        .map(|skip| {
                            json!({
                                "reference": skip.reference,
                                "source": skip.source.as_str(),
                                "reason": skip.reason.as_str(),
                            })
                        })
                        .collect(),
                ),
            );
        }
        map
    }

    fn annotate_metadata(&self, result: &mut oc_tools::ToolResult) {
        let report = self.report_metadata();
        if report.is_empty() {
            return;
        }
        let mut map = match result.metadata.take() {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        map.extend(report);
        result.metadata = Some(Value::Object(map));
    }
}

/// Per-model quota state at spawn time, reported in tool metadata.
#[derive(Debug, Clone)]
struct QuotaSnapshot {
    provider_id: String,
    remaining_pct: Option<f64>,
    limited: bool,
    spend_usd: Option<f64>,
    window_hours: u32,
}

impl QuotaSnapshot {
    fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "providerID".to_owned(),
            Value::String(self.provider_id.clone()),
        );
        if let Some(pct) = self.remaining_pct {
            map.insert("remainingPercent".to_owned(), json!(pct));
        }
        map.insert("limited".to_owned(), Value::Bool(self.limited));
        if let Some(spend) = self.spend_usd {
            map.insert("spendUsd".to_owned(), json!(spend));
        }
        map.insert("windowHours".to_owned(), json!(self.window_hours));
        Value::Object(map)
    }
}

struct SpawnContext {
    runtime: Runtime,
    caller: String,
    /// Parent row id in `session_v2` (equals `caller` for nested children, or
    /// the coordinator's parent session when attaching team workers flatly).
    parent_id: String,
    /// Owning coordinator id when `parent_id != caller` (`metadata.coordinatorID`).
    coordinator_id: Option<String>,
    agent: String,
    settings: SubagentSettings,
    limits: oc_subagents::Limits,
    active_children: u32,
    plan: oc_subagents::ChildPlan,
    parent_root_id: String,
    parent_info: oc_types::SessionInfo,
    model_value: Value,
    created_model: Option<oc_types::ModelRef>,
    model_source: oc_subagents::ModelSource,
    quota_snapshot: Option<QuotaSnapshot>,
    /// Cooling-down keys of the chosen model's provider at spawn time; empty
    /// when every key is healthy.
    key_health: Vec<Value>,
    rejected_models: Vec<oc_subagents::ModelSkip>,
    role: oc_subagents::Role,
    lane: Option<String>,
    group: Option<String>,
}

impl SpawnContext {
    /// Session that receives child completion/failure deliveries and watches.
    pub(super) fn reports_to(&self) -> &str {
        self.coordinator_id.as_deref().unwrap_or(&self.caller)
    }
}

struct SpawnedPartitionChild {
    session_id: String,
    prompt_id: String,
    partition_index: usize,
    partition_count: usize,
    files: Vec<String>,
}

struct PartitionMeta {
    index: usize,
    count: usize,
    files: Vec<String>,
}

enum PartitionPlan {
    None,
    Auto(usize),
    Manual { index: usize, count: usize },
}

fn normalize_partition_args(
    partitions: Option<usize>,
    partition_index: Option<usize>,
    partition_count: Option<usize>,
) -> Result<PartitionPlan, ToolError> {
    match (partitions, partition_index, partition_count) {
        (Some(1), None, None | Some(1)) | (None, Some(0), Some(1)) | (None, None, None) => {
            Ok(PartitionPlan::None)
        }
        (Some(p), Some(idx), count) => {
            if let Some(c) = count
                && c != p
            {
                return Err(ToolError::new(format!(
                    "partitions ({p}) does not match partition_count ({c})"
                )));
            }
            if !(2..=16).contains(&p) {
                return Err(ToolError::new("partitions must be between 2 and 16"));
            }
            Ok(PartitionPlan::Manual {
                index: idx,
                count: p,
            })
        }
        (Some(p), None, count) => {
            if let Some(c) = count
                && c != p
            {
                return Err(ToolError::new(format!(
                    "partitions ({p}) does not match partition_count ({c})"
                )));
            }
            if !(2..=16).contains(&p) {
                return Err(ToolError::new("partitions must be between 2 and 16"));
            }
            Ok(PartitionPlan::Auto(p))
        }
        (None, Some(index), Some(count)) => Ok(PartitionPlan::Manual { index, count }),
        (None, Some(_), None) | (None, None, Some(_)) => Err(ToolError::new(
            "partition_index and partition_count must both be provided together",
        )),
    }
}
