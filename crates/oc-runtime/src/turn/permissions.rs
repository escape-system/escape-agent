use std::path::Path;
use std::sync::Arc;

use oc_config::Effect as ConfigEffect;
use oc_types::MessageId;
use oc_types::message::StructuredError;
use serde_json::{Map, Value};

use crate::runtime::{CancelToken, PermissionAsk, PermissionDecision, Runtime};

use super::types::{AuthFailure, PendingToolCall, ResolvedTurn};

/// Authorize one dispatched tool call.
///
/// `patch` is special-cased because its input carries paths only inside the
/// patch text; [`authorize_patch`] checks every target. Every other tool uses
/// the single `permission_target` resource.
pub(super) async fn authorize_tool_call(
    runtime: &Runtime,
    turn: &ResolvedTurn,
    message_id: &MessageId,
    call: &PendingToolCall,
    cancel: &Arc<CancelToken>,
) -> Result<(), AuthFailure> {
    if call.name == "patch" {
        return authorize_patch(runtime, turn, message_id, call, cancel).await;
    }
    let (action, resource, external) = permission_target(&call.name, &call.input, &turn.directory);
    if let Some(path) = external {
        runtime
            .authorize(
                turn,
                message_id,
                &call.name,
                &call.id,
                "external_directory",
                &path,
                cancel,
            )
            .await?;
    }
    runtime
        .authorize(
            turn, message_id, &call.name, &call.id, &action, &resource, cancel,
        )
        .await
}

/// Authorize every path a `patch` call touches.
///
/// `oc-tools` parses the patch envelope (the tool rejects invalid text before
/// applying anything), so this sees the same Add/Delete/Update/Move paths the
/// tool will write. Each target is checked as `edit` with the same
/// normalization and `external_directory` rule used by `write`/`edit`.
pub(super) async fn authorize_patch(
    runtime: &Runtime,
    turn: &ResolvedTurn,
    message_id: &MessageId,
    call: &PendingToolCall,
    cancel: &Arc<CancelToken>,
) -> Result<(), AuthFailure> {
    let patch_text = call
        .input
        .get("patchText")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for target in oc_tools::patch::patch_targets(patch_text) {
        let path = normalize_path(&turn.directory, &target);
        if !inside(&turn.directory, &path) {
            runtime
                .authorize(
                    turn,
                    message_id,
                    &call.name,
                    &call.id,
                    "external_directory",
                    &path,
                    cancel,
                )
                .await?;
        }
        runtime
            .authorize(
                turn, message_id, &call.name, &call.id, "edit", &path, cancel,
            )
            .await?;
    }
    Ok(())
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    async fn authorize(
        &self,
        turn: &ResolvedTurn,
        message_id: &MessageId,
        tool: &str,
        call_id: &str,
        action: &str,
        resource: &str,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), AuthFailure> {
        match self.effect(&turn.agent_id, &turn.session_rules, action, resource) {
            ConfigEffect::Allow => Ok(()),
            ConfigEffect::Deny => Err(AuthFailure::Denied(StructuredError::new(
                "PermissionDeniedError",
                format!("Permission denied for {action} on {resource}."),
            ))),
            ConfigEffect::Ask => {
                let ask = PermissionAsk {
                    session_id: turn.info.id.to_string(),
                    message_id: message_id.to_string(),
                    tool_call_id: call_id.to_owned(),
                    tool: tool.to_owned(),
                    action: action.to_owned(),
                    resources: vec![resource.to_owned()],
                    message: Some(format!("Allow {action} on {resource}?")),
                };
                let _permission_wait = self.enter_permission_wait(turn.info.id.as_ref());
                let decision = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(AuthFailure::Cancelled),
                    decision = self.inner.config.permissions.resolve(ask) => decision,
                };
                match decision {
                    PermissionDecision::Allow => Ok(()),
                    PermissionDecision::Deny { message } => {
                        let message = if message.trim().is_empty() {
                            format!("Permission denied for {action} on {resource}.")
                        } else {
                            message
                        };
                        Err(AuthFailure::Denied(StructuredError::new(
                            "PermissionDeniedError",
                            message,
                        )))
                    }
                }
            }
        }
    }
}

pub(super) fn tool_action(name: &str) -> &str {
    match name {
        "write" | "edit" | "patch" => "edit",
        "subagent" => "subagent",
        "talk" | "session_write" | "roster" | "digest" | "memory_search" | "memory_list" => "read",
        "memory_write" | "memory_forget" => "memory:write",
        "goal_set" | "goal_update" | "goal_complete" | "goal_status" => "goal",
        "skill" | "skill_lint" => "skill",
        "skill_write" => "skill:write",
        "lane" => "lane",
        other => other,
    }
}

pub(super) fn permission_target(
    tool: &str,
    input: &Map<String, Value>,
    directory: &str,
) -> (String, String, Option<String>) {
    let action = tool_action(tool).to_owned();
    match tool {
        "read" | "write" | "edit" | "patch" | "glob" | "grep" => {
            let raw = input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(directory);
            let path = normalize_path(directory, raw);
            let external = (!inside(directory, &path)).then(|| path.clone());
            (action, path, external)
        }
        "shell" => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (action, command.to_owned(), None)
        }
        "subagent" => {
            let agent = input.get("agent").and_then(Value::as_str).unwrap_or("*");
            (action, agent.to_owned(), None)
        }
        "talk" => {
            let to = input.get("to").and_then(Value::as_str).unwrap_or("*");
            (action, to.to_owned(), None)
        }
        "session_write" | "digest" => {
            let session = input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("*");
            (action, session.to_owned(), None)
        }
        "skill" | "skill_write" | "skill_lint" => {
            let id = input.get("id").and_then(Value::as_str).unwrap_or("*");
            (action, id.to_owned(), None)
        }
        "lane" => {
            let resource = input
                .get("lane")
                .or_else(|| input.get("command"))
                .and_then(Value::as_str)
                .unwrap_or("*");
            (action, resource.to_owned(), None)
        }
        "goal_set" | "goal_update" | "goal_complete" | "goal_status" => {
            (action, "*".to_owned(), None)
        }
        _ => {
            let resource = input
                .get("path")
                .or_else(|| input.get("url"))
                .or_else(|| input.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("*");
            (action, resource.to_owned(), None)
        }
    }
}

fn normalize_path(directory: &str, raw: &str) -> String {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(directory).join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.to_string_lossy().into_owned()
}

pub(super) fn inside(directory: &str, path: &str) -> bool {
    #[cfg(windows)]
    let (base, path) = (
        directory.replace('\\', "/").to_lowercase(),
        path.replace('\\', "/").to_lowercase(),
    );
    #[cfg(not(windows))]
    let (base, path) = (directory.replace('\\', "/"), path.replace('\\', "/"));
    path == base || path.starts_with(&format!("{base}/"))
}
