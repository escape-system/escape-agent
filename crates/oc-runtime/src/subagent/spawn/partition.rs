use oc_tools::{ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::super::wait::{finish_foreground, spawn_watcher};
use super::prepare::{create_and_notify_child, reserve_spawn_slots};
use super::{PartitionMeta, SpawnContext, SpawnedPartitionChild};

pub(super) async fn execute_partitioned(
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
        &ctx_spawn.parent_id,
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
        let report_target = ctx_spawn.reports_to().to_owned();
        for (child, slice_desc) in children {
            spawn_watcher(
                ctx_spawn.runtime.clone(),
                report_target.clone(),
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
        let report_target = ctx_spawn.reports_to().to_owned();
        let agent = ctx_spawn.agent.clone();
        let settings = ctx_spawn.settings.clone();
        let cancel_cloned = cancel.cloned();
        join_set.spawn(async move {
            let res = finish_foreground(
                &runtime,
                &report_target,
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
