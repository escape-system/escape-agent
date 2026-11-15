use std::path::Path;
use std::sync::Arc;

use oc_tools::ToolContext;
use oc_types::MessageId;
use oc_types::message::{StructuredError, ToolContent};
use serde_json::{Map, Value};

use crate::runtime::{CancelToken, Runtime};
use crate::turn::permissions::authorize_tool_call;
use crate::turn::types::{PendingToolCall, ResolvedTurn, ToolOutcome};

use super::StripStartContext;
use super::outcome::{auth_outcome, metadata_map, tool_content};

#[allow(clippy::too_many_lines)]
pub(super) async fn run_tool(
    runtime: Runtime,
    turn: ResolvedTurn,
    message_id: MessageId,
    call: PendingToolCall,
    cancel: Arc<CancelToken>,
    strip_ctx: StripStartContext,
) -> ToolOutcome {
    if let Some(expectation) = &call.expectation {
        tracing::debug!(
            tool = %call.name,
            why = %expectation.why,
            expect_seconds = expectation.expect_seconds,
            "tool expectation"
        );
    }
    if let Some(error) = call.invalid.clone() {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error,
            content: None,
            metadata: None,
            executed: false,
        };
    }

    if crate::turn::session::is_complex_root_restricted_tool(&call.name)
        && !turn.tool_names.iter().any(|name| name == &call.name)
    {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ComplexModePolicyError",
                format!(
                    "Tool {:?} is disabled for the root orchestrator (depth 0) in /complex mode. \
                     Delegate file exploration, edits, and shell execution by spawning a `subagent` \
                     or a `role: \"coordinator\"` subagent.",
                    call.name
                ),
            ),
            content: None,
            metadata: None,
            executed: false,
        };
    }

    if let Err(failure) = authorize_tool_call(&runtime, &turn, &message_id, &call, &cancel).await {
        return auth_outcome(failure, call);
    }

    let Some(tool) = turn.registry.get(&call.name).cloned() else {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ToolNotFoundError",
                format!("Tool {:?} is not registered.", call.name),
            ),
            content: None,
            metadata: None,
            executed: false,
        };
    };

    if let Some(engine) = runtime.observer().as_deref()
        && let Some(strip) = crate::observer::flight_strip(
            &strip_ctx.root_id,
            turn.info.id.as_ref(),
            &turn.agent_id,
            strip_ctx.depth,
            &call,
        )
    {
        strip_ctx
            .open
            .lock()
            .expect("open strips")
            .push(call.id.clone());
        engine.observe_start(strip);
    }

    let tool_token = cancel.token.child_token();
    let _tool_cancel = runtime.register_tool_cancel(
        &call.id,
        turn.info.id.as_ref(),
        &call.name,
        tool_token.clone(),
    );

    let context = ToolContext {
        session_id: turn.info.id.to_string(),
        directory: Path::new(&turn.directory).to_path_buf(),
        cwd: Path::new(&turn.directory).to_path_buf(),
        cancel: Some(tool_token.clone()),
    };
    let input = Value::Object(call.input.clone());
    crate::snapshot::capture_before_tool(
        &runtime,
        turn.info.id.as_ref(),
        message_id.as_ref(),
        &call.name,
        &call.id,
        &input,
        &turn.directory,
    )
    .await;
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return ToolOutcome::Cancelled,
        () = tool_token.cancelled() => {
            if cancel.is_cancelled() {
                return ToolOutcome::Cancelled;
            }
            return ToolOutcome::Failed {
                index: call.index,
                input: call.input,
                error: StructuredError::new(
                    "ToolCancelledError",
                    format!(
                        "Tool call {:?} ({}) was cancelled by observer repair.",
                        call.id, call.name
                    ),
                ),
                content: None,
                metadata: None,
                executed: true,
            };
        }
        result = oc_tools::code_map::with_semantic_oracle(
            runtime.semantic_oracle(),
            oc_tools::key_health::with_key_health_source(
                Some(runtime.key_health_source()),
                tool.execute(&context, input),
            ),
        ) => result,
    };
    if tool_token.is_cancelled() && !cancel.is_cancelled() {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ToolCancelledError",
                format!(
                    "Tool call {:?} ({}) was cancelled by observer repair.",
                    call.id, call.name
                ),
            ),
            content: None,
            metadata: None,
            executed: true,
        };
    }
    match result {
        Ok(result) => {
            let (mut tool_parts, mut tool_metadata) =
                tool_content(&result, &turn.directory, &call.id);
            maybe_attach_post_edit_intelligence(
                &runtime,
                &turn.directory,
                &call.name,
                &call.input,
                &mut tool_parts,
                &mut tool_metadata,
            )
            .await;
            ToolOutcome::Completed {
                index: call.index,
                input: call.input,
                content: tool_parts,
                metadata: tool_metadata,
                executed: true,
            }
        }
        Err(error) => ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new("ToolError", error.message),
            content: None,
            metadata: metadata_map(error.metadata.as_ref()),
            executed: true,
        },
    }
}

async fn maybe_attach_post_edit_intelligence(
    runtime: &Runtime,
    directory: &str,
    tool_name: &str,
    input: &Map<String, Value>,
    parts: &mut Vec<ToolContent>,
    metadata: &mut Option<Map<String, Value>>,
) {
    let root = Path::new(directory);
    let changed = collect_mutated_code_files(root, tool_name, input, metadata.as_ref());
    if changed.is_empty() {
        return;
    }

    let hook = runtime.edit_diagnostics();
    let lsp_fut = async {
        match hook {
            Some(h) => Some(h.after_edit(root, &changed).await),
            None => None,
        }
    };
    let radar_fut = oc_tools::code_map::collect_post_edit_radar(root, &changed, false);
    let (lsp_diag, radar) = tokio::join!(lsp_fut, radar_fut);

    if let Some(diag) = lsp_diag {
        if let Some(text) = diag
            .formatted
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            append_tool_supplement(parts, text);
        }
        if let Some(diag_meta) = diag.metadata {
            metadata
                .get_or_insert_with(Map::new)
                .insert("diagnostics".to_owned(), diag_meta);
        }
    }
    if let Some(radar_text) = radar.render_text() {
        append_tool_supplement(parts, &radar_text);
    }
    if let Some(radar_meta) = radar.to_metadata() {
        metadata
            .get_or_insert_with(Map::new)
            .insert("radar".to_owned(), radar_meta);
    }
}

fn collect_mutated_code_files(
    root: &Path,
    tool_name: &str,
    input: &Map<String, Value>,
    metadata: Option<&Map<String, Value>>,
) -> Vec<String> {
    if !matches!(tool_name, "write" | "edit" | "patch" | "symbol_edit") {
        return Vec::new();
    }
    if tool_name == "symbol_edit"
        && input.get("operation").and_then(Value::as_str) == Some("preview")
    {
        return Vec::new();
    }

    let mut out = Vec::new();
    if let Some(files) = metadata
        .and_then(|m| m.get("files"))
        .and_then(Value::as_array)
    {
        for entry in files {
            if let Some(file) = entry.get("file").and_then(Value::as_str) {
                push_unique_code_file(root, file, &mut out);
            }
        }
    }
    if !out.is_empty() {
        return out;
    }

    match tool_name {
        "write" | "edit" => {
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                push_unique_code_file(root, path, &mut out);
            }
        }
        "patch" => {
            if let Some(patch_text) = input.get("patchText").and_then(Value::as_str) {
                for target in oc_tools::patch::patch_targets(patch_text) {
                    push_unique_code_file(root, &target, &mut out);
                }
            }
        }
        "symbol_edit" => {
            if let Some(path) =
                crate::snapshot::symbol_edit_target_path(&Value::Object(input.clone()))
            {
                push_unique_code_file(root, &path, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn push_unique_code_file(root: &Path, raw: &str, out: &mut Vec<String>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let normalized = Path::new(trimmed).strip_prefix(root).map_or_else(
        |_| trimmed.to_owned(),
        |rel| rel.to_string_lossy().into_owned(),
    );
    if !normalized.is_empty()
        && crate::turn::verify::is_code_file(&normalized)
        && !out.iter().any(|existing| existing == &normalized)
    {
        out.push(normalized);
    }
}

fn append_tool_supplement(parts: &mut Vec<ToolContent>, supplement: &str) {
    if let Some(ToolContent::Text { text }) = parts
        .iter_mut()
        .rev()
        .find(|part| matches!(part, ToolContent::Text { .. }))
    {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(supplement);
    } else {
        parts.push(ToolContent::Text {
            text: supplement.to_owned(),
        });
    }
}
