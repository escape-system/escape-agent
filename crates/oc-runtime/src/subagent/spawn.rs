use std::time::Duration;

use oc_store::{NewSession, NewSessionTree};
use oc_subagents::{BudgetError, Policy, SpawnError, resolve_model};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::SessionId;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

use super::settings::{
    SubagentSettings, available_agents, available_models, model_defaults, model_value,
    requested_role, settings_for_session,
};
use super::tree::{
    budget_error, load_info, load_node, load_tree, optional_bool, optional_str, required_str,
    runtime_error, spawn_error, store_error, write_error,
};
use super::wait::{enqueue_prompt, finish_foreground, spawn_watcher};

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
                "sessionID": { "type": "string", "description": "Continue an existing direct child or descendant. Omit when spawning a new child." },
                "background": { "type": "boolean", "description": self.background_description() },
                "role": { "type": "string", "enum": ["worker", "coordinator"], "description": "Child policy role." },
                "spawn": {
                    "type": "object",
                    "properties": {
                        "allow": { "type": "boolean", "description": "Allow the child to spawn its own children." },
                        "max_depth": { "type": "integer", "minimum": 0, "description": "Absolute tree depth cap for this subtree (must be >= this child's depth, i.e. parent.depth + 1). Omit to use the configured default." }
                    },
                    "additionalProperties": false
                },
                "lane": { "type": "string", "description": "Lane hint recorded for the lane service." },
                "group": { "type": "string", "description": "Optional group label recorded in session metadata (docs/21-grouping.md)." },
                "partitions": {
                    "type": "integer",
                    "minimum": 2,
                    "maximum": 16,
                    "description": "Automatically partition the task via `code_map` graph clustering into N disjoint slices (each receiving `<shared_core>` + its `<partition>`) and launch N parallel workers."
                },
                "partition_index": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 15,
                    "description": "Manual 0-based partition slice index when spawning a single worker with `partition_count`."
                },
                "partition_count": {
                    "type": "integer",
                    "minimum": 2,
                    "maximum": 16,
                    "description": "Total manual partition count paired with `partition_index`."
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
        let settings = settings_for_session(runtime, &caller).await;
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

        if let Some(count) = partitions {
            if !(2..=16).contains(&count) {
                return Err(ToolError::new("partitions must be between 2 and 16"));
            }
            if partition_index.is_some() || partition_count.is_some() {
                return Err(ToolError::new(
                    "partitions cannot be combined with partition_index or partition_count",
                ));
            }
        }

        let manual_slice =
            resolve_manual_partition(ctx, &prompt, partition_index, partition_count).await?;

        let ctx_spawn = prepare_spawn_context(runtime, &caller, &agent, &input, &settings).await?;

        if let Some(count) = partitions {
            return execute_partitioned(ctx, &ctx_spawn, &description, &prompt, count, background)
                .await;
        }

        reserve_spawn_slots(
            runtime,
            &caller,
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

        if background {
            spawn_watcher(
                runtime.clone(),
                caller.clone(),
                child.session_id.clone(),
                agent,
                description,
                child.prompt_id,
                settings.timeout,
            );
            return Ok(ToolResult::text(
                json!({ "sessionID": child.session_id, "status": "running" }).to_string(),
            )
            .with_metadata(json!({ "sessionID": child.session_id, "status": "running" })));
        }
        let mut running_meta = serde_json::Map::new();
        running_meta.insert(
            "sessionID".to_owned(),
            Value::String(child.session_id.clone()),
        );
        running_meta.insert("status".to_owned(), Value::String("running".to_owned()));
        crate::turn::report_tool_progress(running_meta);
        finish_foreground(
            runtime,
            &caller,
            &child.session_id,
            &agent,
            &description,
            &child.prompt_id,
            &settings,
            ctx.cancel.as_ref(),
        )
        .await
    }
}

struct SpawnContext {
    runtime: Runtime,
    caller: String,
    agent: String,
    settings: SubagentSettings,
    limits: oc_subagents::Limits,
    active_children: u32,
    plan: oc_subagents::ChildPlan,
    parent_root_id: String,
    parent_info: oc_types::SessionInfo,
    model_value: Value,
    created_model: Option<oc_types::ModelRef>,
    role: oc_subagents::Role,
    lane: Option<String>,
    group: Option<String>,
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

fn parse_optional_usize(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    let Some(val) = input.get(key) else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let Some(n) = val.as_u64() else {
        return Err(ToolError::new(format!(
            "{key} must be a non-negative integer"
        )));
    };
    usize::try_from(n)
        .map(Some)
        .map_err(|_| ToolError::new(format!("{key} is too large")))
}

async fn resolve_manual_partition(
    ctx: &ToolContext,
    prompt: &str,
    partition_index: Option<usize>,
    partition_count: Option<usize>,
) -> Result<Option<(String, PartitionMeta)>, ToolError> {
    match (partition_index, partition_count) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(ToolError::new(
            "partition_index and partition_count must both be provided together",
        )),
        (Some(idx), Some(count)) => {
            if !(2..=16).contains(&count) {
                return Err(ToolError::new("partition_count must be between 2 and 16"));
            }
            if idx >= count {
                return Err(ToolError::new(format!(
                    "partition_index ({idx}) must be less than partition_count ({count})"
                )));
            }
            let partitioned =
                oc_tools::code_map::partition_task(ctx, &ctx.directory, prompt, count, 2000)
                    .await?;
            let section = partitioned.prompt_section_for(idx).unwrap_or_default();
            let files = partitioned
                .slices
                .iter()
                .find(|s| s.index == idx)
                .map(|s| s.files.clone())
                .unwrap_or_default();
            Ok(Some((
                section,
                PartitionMeta {
                    index: idx,
                    count: partitioned.slices.len().max(1),
                    files,
                },
            )))
        }
    }
}

async fn prepare_spawn_context(
    runtime: &Runtime,
    caller: &str,
    agent: &str,
    input: &Value,
    settings: &SubagentSettings,
) -> Result<SpawnContext, ToolError> {
    let mut parent = load_node(runtime, caller).await?;
    let mut limits = settings.limits.clone();
    if let Some(over) = settings.overrides.get(&parent.agent) {
        if over.allow_spawn == Some(false) {
            return Err(spawn_error(&SpawnError::SpawnNotAllowed {
                id: caller.to_owned(),
            }));
        }
        if let Some(max_children) = over.max_children {
            limits.max_children = max_children;
        }
    }
    let active = runtime
        .inner
        .config
        .store
        .active_child_count(caller)
        .await
        .unwrap_or(parent.child_count);
    if active < parent.child_count {
        parent.child_count = active;
    }
    let available = available_agents(&runtime.inner.config.config);
    let policy = Policy::new(limits.clone());
    let requested_depth = input
        .get("spawn")
        .and_then(|spawn| spawn.get("max_depth"))
        .and_then(Value::as_u64)
        .map(|depth| u32::try_from(depth).unwrap_or(u32::MAX));
    let mut plan = policy
        .spawn(&parent, agent, requested_depth, &available)
        .map_err(|error| spawn_error(&error))?;

    if let Some(over) = settings.overrides.get(agent)
        && over.allow_spawn == Some(false)
    {
        plan.spawn_allowed = false;
    }
    if input
        .get("spawn")
        .and_then(|spawn| spawn.get("allow"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        plan.spawn_allowed = false;
    }

    let parent_model = parent
        .model
        .clone()
        .or_else(|| runtime.inner.config.config.model())
        .unwrap_or_default();
    let defaults = model_defaults(&runtime.inner.config.config, settings);
    let choice = resolve_model(
        optional_str(input, "model").as_deref(),
        Some(agent),
        defaults,
        &parent_model,
        &available_models(&runtime.inner.config.catalog),
    )
    .map_err(|error| ToolError::new(format!("ModelUnavailableError: {error}")))?;
    let model_value = model_value(&choice.reference, &runtime.inner.config.catalog)?;
    let created_model = serde_json::from_value(model_value.clone()).ok();
    let parent_info = load_info(runtime, caller).await?;
    let role = requested_role(input, settings, agent);
    let lane = optional_str(input, "lane");
    let group = optional_str(input, "group").and_then(|g| {
        let trimmed = g.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });

    Ok(SpawnContext {
        runtime: runtime.clone(),
        caller: caller.to_owned(),
        agent: agent.to_owned(),
        settings: settings.clone(),
        limits,
        active_children: parent.child_count,
        plan,
        parent_root_id: parent.root_id,
        parent_info,
        model_value,
        created_model,
        role,
        lane,
        group,
    })
}

async fn reserve_spawn_slots(
    runtime: &Runtime,
    caller: &str,
    limits: &oc_subagents::Limits,
    active_children: u32,
    count: u32,
    cancel: Option<&CancellationToken>,
) -> Result<(), ToolError> {
    loop {
        let result = {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .spawn_budget(caller, limits, active_children)
                .try_spawn_batch(count, oc_types::now_millis())
        };
        match result {
            Ok(()) => return Ok(()),
            Err(
                ref error @ BudgetError::RateLimited {
                    retry_after_ms,
                    max_per_minute,
                    ..
                },
            ) if max_per_minute >= count && retry_after_ms > 0 && retry_after_ms <= 25_000 => {
                let wait_ms = u64::try_from(retry_after_ms.saturating_add(25)).unwrap_or(25);
                let wait = Duration::from_millis(wait_ms);
                if let Some(cancel) = cancel {
                    tokio::select! {
                        () = cancel.cancelled() => return Err(budget_error(error)),
                        () = tokio::time::sleep(wait) => {}
                    }
                } else {
                    tokio::time::sleep(wait).await;
                }
            }
            Err(error) => return Err(budget_error(&error)),
        }
    }
}

async fn create_and_notify_child(
    ctx_spawn: &SpawnContext,
    description: &str,
    prompt: &str,
    partition: Option<&PartitionMeta>,
) -> Result<SpawnedPartitionChild, ToolError> {
    let runtime = &ctx_spawn.runtime;
    let child = SessionId::new().into_inner();
    let mut meta = serde_json::Map::new();
    if let Some(lane) = &ctx_spawn.lane {
        meta.insert("lane".to_owned(), Value::String(lane.clone()));
    }
    if let Some(group) = &ctx_spawn.group {
        meta.insert("group".to_owned(), Value::String(group.clone()));
    }
    if let Some(part) = partition {
        meta.insert("partition_index".to_owned(), json!(part.index));
        meta.insert("partition_count".to_owned(), json!(part.count));
        meta.insert("partition_files".to_owned(), json!(part.files));
    }
    let metadata = if meta.is_empty() {
        None
    } else {
        Some(Value::Object(meta))
    };
    let created_metadata = metadata.as_ref().and_then(Value::as_object).cloned();

    runtime
        .inner
        .config
        .store
        .create_session_with_tree(
            NewSession {
                id: child.clone(),
                parent_id: Some(ctx_spawn.caller.clone()),
                project_id: ctx_spawn.parent_info.project_id.to_string(),
                directory: ctx_spawn.parent_info.location.directory.clone(),
                title: Some(description.to_owned()),
                agent: Some(ctx_spawn.agent.clone()),
                model: Some(ctx_spawn.model_value.clone()),
                metadata,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: ctx_spawn.parent_root_id.clone(),
                depth: ctx_spawn.plan.depth,
                role: ctx_spawn.role.as_str().to_owned(),
                spawn_allowed: ctx_spawn.plan.spawn_allowed,
            },
        )
        .await
        .map_err(|error| store_error(&error))?;

    runtime
        .emit(
            &child,
            &ctx_spawn.parent_info.location.directory,
            oc_types::payload::SessionCreated {
                session_id: SessionId::from_wire(child.clone()),
                project_id: ctx_spawn.parent_info.project_id.clone(),
                location: ctx_spawn.parent_info.location.clone(),
                subpath: ctx_spawn.parent_info.subpath.clone(),
                parent_id: Some(SessionId::from_wire(ctx_spawn.caller.clone())),
                slug: format!("subagent-{}", &child[child.len().saturating_sub(6)..]),
                title: Some(description.to_owned()),
                agent: Some(ctx_spawn.agent.clone()),
                model: ctx_spawn.created_model.clone(),
                metadata: created_metadata,
                permissions: None,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        )
        .await
        .map_err(|error| runtime_error(&error))?;

    let prompt_id = enqueue_prompt(runtime, &child, prompt, &ctx_spawn.caller).await?;
    runtime
        .notify(&child)
        .await
        .map_err(|error| runtime_error(&error))?;

    Ok(SpawnedPartitionChild {
        session_id: child,
        prompt_id,
        partition_index: partition.map_or(0, |p| p.index),
        partition_count: partition.map_or(1, |p| p.count),
        files: partition.map(|p| p.files.clone()).unwrap_or_default(),
    })
}

async fn execute_partitioned(
    tool_ctx: &ToolContext,
    ctx_spawn: &SpawnContext,
    description: &str,
    prompt: &str,
    requested_partitions: usize,
    background: bool,
) -> Result<ToolResult, ToolError> {
    let partitioned = oc_tools::code_map::partition_task(
        tool_ctx,
        &tool_ctx.directory,
        prompt,
        requested_partitions,
        2000,
    )
    .await?;

    let slices = if partitioned.slices.is_empty() {
        vec![oc_tools::code_map::PartitionSlice {
            index: 0,
            files: Vec::new(),
            symbols: Vec::new(),
            text: String::new(),
        }]
    } else {
        partitioned.slices.clone()
    };
    let count = slices.len();
    let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);

    reserve_spawn_slots(
        &ctx_spawn.runtime,
        &ctx_spawn.caller,
        &ctx_spawn.limits,
        ctx_spawn.active_children,
        count_u32,
        tool_ctx.cancel.as_ref(),
    )
    .await?;

    let mut children = Vec::with_capacity(count);
    for slice in &slices {
        let slice_desc = format!("{description} [{}/{}]", slice.index + 1, count);
        let slice_prompt = partitioned
            .prompt_section_for(slice.index)
            .filter(|s| !s.trim().is_empty())
            .map_or_else(|| prompt.to_owned(), |sec| format!("{prompt}\n\n{sec}"));
        let pmeta = PartitionMeta {
            index: slice.index,
            count,
            files: slice.files.clone(),
        };
        let spawned =
            create_and_notify_child(ctx_spawn, &slice_desc, &slice_prompt, Some(&pmeta)).await?;
        children.push((spawned, slice_desc));
    }

    if background {
        let mut sessions_meta = Vec::with_capacity(children.len());
        for (child, slice_desc) in children {
            spawn_watcher(
                ctx_spawn.runtime.clone(),
                ctx_spawn.caller.clone(),
                child.session_id.clone(),
                ctx_spawn.agent.clone(),
                slice_desc,
                child.prompt_id,
                ctx_spawn.settings.timeout,
            );
            sessions_meta.push(json!({
                "sessionID": child.session_id,
                "partition_index": child.partition_index,
                "partition_count": child.partition_count,
                "files": child.files,
                "status": "running",
            }));
        }
        let payload = json!({
            "status": "running",
            "partitions": count,
            "engine": partitioned.engine,
            "sessions": sessions_meta,
        });
        return Ok(ToolResult::text(payload.to_string()).with_metadata(payload));
    }

    wait_partitioned_foreground(
        ctx_spawn,
        &partitioned.engine,
        children,
        tool_ctx.cancel.as_ref(),
    )
    .await
}

async fn wait_partitioned_foreground(
    ctx_spawn: &SpawnContext,
    engine: &str,
    children: Vec<(SpawnedPartitionChild, String)>,
    cancel: Option<&CancellationToken>,
) -> Result<ToolResult, ToolError> {
    let session_progress: Vec<Value> = children
        .iter()
        .map(|(child, _)| {
            json!({
                "sessionID": child.session_id,
                "partition_index": child.partition_index,
                "partition_count": child.partition_count,
                "files": child.files,
                "status": "running",
            })
        })
        .collect();
    let mut progress_meta = serde_json::Map::new();
    progress_meta.insert("status".to_owned(), json!("running"));
    progress_meta.insert("partitions".to_owned(), json!(children.len()));
    progress_meta.insert("sessions".to_owned(), Value::Array(session_progress));
    crate::turn::report_tool_progress(progress_meta);

    let mut join_set = tokio::task::JoinSet::new();
    for (child, slice_desc) in children {
        let runtime = ctx_spawn.runtime.clone();
        let caller = ctx_spawn.caller.clone();
        let agent = ctx_spawn.agent.clone();
        let settings = ctx_spawn.settings.clone();
        let cancel_cloned = cancel.cloned();
        join_set.spawn(async move {
            let res = finish_foreground(
                &runtime,
                &caller,
                &child.session_id,
                &agent,
                &slice_desc,
                &child.prompt_id,
                &settings,
                cancel_cloned.as_ref(),
            )
            .await;
            (child, res)
        });
    }

    let mut completed = Vec::new();
    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok(pair) => completed.push(pair),
            Err(err) => {
                return Err(ToolError::new(format!(
                    "partitioned subagent task aborted: {err}"
                )));
            }
        }
    }
    completed.sort_by_key(|(child, _)| child.partition_index);
    format_partition_results(engine, completed)
}

fn format_partition_results(
    engine: &str,
    completed: Vec<(SpawnedPartitionChild, Result<ToolResult, ToolError>)>,
) -> Result<ToolResult, ToolError> {
    let mut sections = Vec::with_capacity(completed.len());
    let mut sessions_meta = Vec::with_capacity(completed.len());
    let mut succeeded = 0usize;
    let total = completed.len();

    for (child, outcome) in completed {
        let (status, body) = match outcome {
            Ok(res) => {
                let status = res
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_owned();
                if status == "completed" {
                    succeeded += 1;
                }
                (status, res.output.unwrap_or_default())
            }
            Err(err) => ("failed".to_owned(), err.message),
        };
        let files_csv = child.files.join(",");
        sections.push(format!(
            "<subagent-partition index=\"{}\" session=\"{}\" status=\"{}\" files=\"{files_csv}\">\n{body}\n</subagent-partition>",
            child.partition_index, child.session_id, status
        ));
        sessions_meta.push(json!({
            "sessionID": child.session_id,
            "partition_index": child.partition_index,
            "partition_count": child.partition_count,
            "files": child.files,
            "status": status,
        }));
    }

    let overall_status = if succeeded == total {
        "completed"
    } else if succeeded > 0 {
        "partial"
    } else {
        "failed"
    };
    let meta = json!({
        "status": overall_status,
        "partitions": total,
        "engine": engine,
        "sessions": sessions_meta,
    });
    let combined = sections.join("\n\n");
    if succeeded == 0 {
        Err(ToolError::new(combined).with_metadata(meta))
    } else {
        Ok(ToolResult::text(combined).with_metadata(meta))
    }
}

#[allow(clippy::too_many_arguments)]
async fn continue_child(
    runtime: &Runtime,
    caller: &str,
    target: &str,
    prompt: &str,
    description: &str,
    background: bool,
    settings: &SubagentSettings,
    cancel: Option<&CancellationToken>,
) -> Result<ToolResult, ToolError> {
    let caller_node = load_node(runtime, caller).await?;
    let target_node = load_node(runtime, target).await?;
    let tree = load_tree(runtime, &[caller.to_owned(), target.to_owned()]).await?;
    Policy::new(settings.limits.clone())
        .can_write(&caller_node, &target_node, &tree)
        .map_err(|error| write_error(&error))?;
    let prompt_id = enqueue_prompt(runtime, target, prompt, caller).await?;
    runtime
        .notify(target)
        .await
        .map_err(|error| runtime_error(&error))?;
    if background {
        spawn_watcher(
            runtime.clone(),
            caller.to_owned(),
            target.to_owned(),
            target_node.agent.clone(),
            description.to_owned(),
            prompt_id,
            settings.timeout,
        );
        return Ok(ToolResult::text(
            json!({ "sessionID": target, "status": "running" }).to_string(),
        )
        .with_metadata(json!({ "sessionID": target, "status": "running" })));
    }
    let mut running_meta = serde_json::Map::new();
    running_meta.insert("sessionID".to_owned(), Value::String(target.to_owned()));
    running_meta.insert("status".to_owned(), Value::String("running".to_owned()));
    crate::turn::report_tool_progress(running_meta);
    finish_foreground(
        runtime,
        caller,
        target,
        &target_node.agent,
        description,
        &prompt_id,
        settings,
        cancel,
    )
    .await
}
