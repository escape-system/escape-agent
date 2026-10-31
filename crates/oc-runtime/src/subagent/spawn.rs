use oc_store::{NewSession, NewSessionTree};
use oc_subagents::{Policy, SpawnError, resolve_model};
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
                "model": { "type": "string", "description": "Model override: provider/id or provider/id#variant." },
                "sessionID": { "type": "string", "description": "Continue an existing direct child or descendant." },
                "background": { "type": "boolean", "description": self.background_description() },
                "role": { "type": "string", "enum": ["worker", "coordinator"], "description": "Child policy role." },
                "spawn": {
                    "type": "object",
                    "properties": {
                        "allow": { "type": "boolean", "description": "Allow the child to spawn its own children." },
                        "max_depth": { "type": "integer", "minimum": 0 }
                    },
                    "additionalProperties": false
                },
                "lane": { "type": "string", "description": "Lane hint recorded for the lane service." }
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

        if let Some(target) = optional_str(&input, "sessionID") {
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

        let mut parent = load_node(runtime, &caller).await?;
        let mut limits = settings.limits.clone();
        if let Some(over) = settings.overrides.get(&parent.agent) {
            if over.allow_spawn == Some(false) {
                return Err(spawn_error(&SpawnError::SpawnNotAllowed { id: caller }));
            }
            if let Some(max_children) = over.max_children {
                limits.max_children = max_children;
            }
        }
        if parent.child_count >= limits.max_children {
            let active = runtime
                .inner
                .config
                .store
                .active_child_count(&caller)
                .await
                .unwrap_or(parent.child_count);
            if active < limits.max_children {
                parent.child_count = active;
            }
        }
        let available = available_agents(&runtime.inner.config.config);
        let policy = Policy::new(limits.clone());
        let requested_depth = input
            .get("spawn")
            .and_then(|spawn| spawn.get("max_depth"))
            .and_then(Value::as_u64)
            .map(|depth| u32::try_from(depth).unwrap_or(u32::MAX));
        let mut plan = policy
            .spawn(&parent, &agent, requested_depth, &available)
            .map_err(|error| spawn_error(&error))?;

        if let Some(over) = settings.overrides.get(&agent)
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

        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .spawn_budget(&caller, &limits, parent.child_count)
                .try_spawn(oc_types::now_millis())
                .map_err(|error| budget_error(&error))?;
        }

        let parent_model = parent
            .model
            .clone()
            .or_else(|| runtime.inner.config.config.model())
            .unwrap_or_default();
        let defaults = model_defaults(&runtime.inner.config.config, &settings);
        let choice = resolve_model(
            optional_str(&input, "model").as_deref(),
            Some(&agent),
            defaults,
            &parent_model,
            &available_models(&runtime.inner.config.catalog),
        )
        .map_err(|error| ToolError::new(format!("ModelUnavailableError: {error}")))?;
        let model_value = model_value(&choice.reference, &runtime.inner.config.catalog)?;
        let created_model = serde_json::from_value(model_value.clone()).ok();
        let parent_info = load_info(runtime, &caller).await?;
        let child = SessionId::new().into_inner();
        let role = requested_role(&input, &settings, &agent);
        let lane = optional_str(&input, "lane");
        let metadata = lane.map(|lane| json!({ "lane": lane }));
        let created_metadata = metadata.as_ref().and_then(Value::as_object).cloned();
        runtime
            .inner
            .config
            .store
            .create_session_with_tree(
                NewSession {
                    id: child.clone(),
                    parent_id: Some(caller.clone()),
                    project_id: parent_info.project_id.to_string(),
                    directory: parent_info.location.directory.clone(),
                    title: Some(description.clone()),
                    agent: Some(agent.clone()),
                    model: Some(model_value),
                    metadata,
                    permissions: None,
                    time_created: oc_types::now_millis(),
                },
                NewSessionTree {
                    root_id: parent.root_id.clone(),
                    depth: plan.depth,
                    role: role.as_str().to_owned(),
                    spawn_allowed: plan.spawn_allowed,
                },
            )
            .await
            .map_err(|error| store_error(&error))?;
        runtime
            .emit(
                &child,
                &parent_info.location.directory,
                oc_types::payload::SessionCreated {
                    session_id: SessionId::from_wire(child.clone()),
                    project_id: parent_info.project_id.clone(),
                    location: parent_info.location.clone(),
                    subpath: parent_info.subpath.clone(),
                    parent_id: Some(SessionId::from_wire(caller.clone())),
                    slug: format!("subagent-{}", &child[child.len().saturating_sub(6)..]),
                    title: Some(description.clone()),
                    agent: Some(agent.clone()),
                    model: created_model,
                    metadata: created_metadata,
                    permissions: None,
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                },
            )
            .await
            .map_err(|error| runtime_error(&error))?;

        let prompt_id = enqueue_prompt(runtime, &child, &prompt, &caller).await?;
        runtime
            .notify(&child)
            .await
            .map_err(|error| runtime_error(&error))?;

        if background {
            spawn_watcher(
                runtime.clone(),
                caller.clone(),
                child.clone(),
                agent,
                description,
                prompt_id,
                settings.timeout,
            );
            return Ok(ToolResult::text(
                json!({ "sessionID": child, "status": "running" }).to_string(),
            )
            .with_metadata(json!({ "sessionID": child, "status": "running" })));
        }
        let mut running_meta = serde_json::Map::new();
        running_meta.insert("sessionID".to_owned(), Value::String(child.clone()));
        running_meta.insert("status".to_owned(), Value::String("running".to_owned()));
        crate::turn::report_tool_progress(running_meta);
        finish_foreground(
            runtime,
            &caller,
            &child,
            &agent,
            &description,
            &prompt_id,
            &settings,
            ctx.cancel.as_ref(),
        )
        .await
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
