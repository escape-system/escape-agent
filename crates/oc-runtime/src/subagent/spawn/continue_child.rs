use oc_subagents::Policy;
use oc_tools::{ToolError, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

use super::super::settings::SubagentSettings;
use super::super::tree::{load_node, load_tree, runtime_error, write_error};
use super::super::wait::{enqueue_prompt, finish_foreground, spawn_watcher};

#[allow(clippy::too_many_arguments)]
pub(super) async fn continue_child(
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
    let prompt_id = enqueue_prompt(runtime, target, prompt, caller, None).await?;
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
