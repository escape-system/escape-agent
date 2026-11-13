//! `map` operation: persistent `ripwire --mcp` first, then CLI fallback, then
//! the graph-centrality native engine.

use std::path::Path;

use serde_json::json;

use super::CodeMapTool;
use super::graph_ops::{McpAttempt, add_fallback_metadata, run_mcp_mode, timeout_error};
use super::native_index::{native_map, relative_scope, tree_root_for};
use super::precision::{PrecisionMode, append_lsp_precision, scip_for_mode};
use super::ripwire::{
    RipwireFailure, compact_ripwire_cli_output, find_ripwire_binary, run_ripwire,
    trim_analyze_xml_to_budget, trimmed_stderr,
};
use crate::{ToolContext, ToolError, ToolResult};

impl CodeMapTool {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn execute_map(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        rank_by: Option<&str>,
        max_tokens: usize,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let ripwire_binary = find_ripwire_binary();
        let root = tree_root_for(ctx, target_path);
        let rel_scope = relative_scope(&root, target_path);

        let mut ripwire_failure: Option<String> = None;
        let mut ripwire_stderr: Option<String> = None;

        if let Some(ripwire_path) = ripwire_binary.as_ref() {
            let (mcp_tool, mcp_args) = if let Some(mode) = rank_by {
                let ripwire_mode = match mode {
                    "coupling" => "hub",
                    "risk" => "rrf",
                    "churn" => "churn",
                    _ => "pagerank",
                };
                (
                    "rank_by",
                    json!({
                        "path": root,
                        "rank_by": ripwire_mode,
                    }),
                )
            } else if query.is_empty() {
                ("analyze", json!({ "path": root }))
            } else {
                let scoped_task = match &rel_scope {
                    Some(rel) => format!("{query} (scope: {rel})"),
                    None => query.to_string(),
                };
                (
                    "for",
                    json!({
                        "path": root,
                        "task": scoped_task,
                        "budget_tokens": max_tokens,
                    }),
                )
            };

            let (mcp_attempt, used_scip) =
                run_mcp_mode(ctx, &root, precision, "map", mcp_tool, &mcp_args).await?;
            match mcp_attempt {
                Some(McpAttempt::Ok(out)) => {
                    ripwire_stderr = trimmed_stderr(&out.stderr);
                    if !out.text.trim().is_empty() {
                        let mut formatted = if query.is_empty() || rank_by.is_some() {
                            trim_analyze_xml_to_budget(
                                &out.text,
                                rel_scope.as_deref(),
                                max_tokens,
                                limit,
                            )
                        } else {
                            compact_ripwire_cli_output(&out.text)
                        };
                        let mut metadata = json!({
                            "operation": "map",
                            "engine": "ripwire",
                            "transport": "mcp",
                            "query": query,
                            "token_budget": max_tokens,
                        });
                        if used_scip {
                            metadata["scip_overlay"] = json!(true);
                        }
                        if precision != PrecisionMode::Fast {
                            metadata["precision"] = json!(precision.as_str());
                        }
                        if let Some(mode) = rank_by {
                            metadata["rank_by"] = json!(mode);
                        }
                        if query.is_empty() {
                            metadata["limit"] = json!(limit);
                        }
                        if let Some(stderr) = ripwire_stderr {
                            metadata["ripwire_stderr"] = json!(stderr);
                        }
                        if !query.is_empty() {
                            append_lsp_precision(
                                ctx,
                                target_path,
                                "map",
                                query,
                                precision,
                                &mut formatted,
                                &mut metadata,
                            )
                            .await;
                        }
                        return Ok(ToolResult::text(formatted).with_metadata(metadata));
                    }
                    ripwire_failure = Some("ripwire returned no output".to_string());
                }
                Some(McpAttempt::Fallback { error, stderr }) => {
                    ripwire_stderr = trimmed_stderr(&stderr);
                    // When `rank_by=churn` refuses (e.g. repo without git), retry `analyze` via MCP.
                    if rank_by.is_some()
                        && mcp_tool == "rank_by"
                        && let (Some(McpAttempt::Ok(analyze_out)), retry_scip) = run_mcp_mode(
                            ctx,
                            &root,
                            precision,
                            "map",
                            "analyze",
                            &json!({ "path": root }),
                        )
                        .await?
                        && !analyze_out.text.trim().is_empty()
                    {
                        let mut formatted = trim_analyze_xml_to_budget(
                            &analyze_out.text,
                            rel_scope.as_deref(),
                            max_tokens,
                            limit,
                        );
                        let mut metadata = json!({
                            "operation": "map",
                            "engine": "ripwire",
                            "transport": "mcp",
                            "query": query,
                            "token_budget": max_tokens,
                            "limit": limit,
                        });
                        if retry_scip {
                            metadata["scip_overlay"] = json!(true);
                        }
                        if precision != PrecisionMode::Fast {
                            metadata["precision"] = json!(precision.as_str());
                        }
                        if let Some(s) =
                            ripwire_stderr.or_else(|| trimmed_stderr(&analyze_out.stderr))
                        {
                            metadata["ripwire_stderr"] = json!(s);
                        }
                        if !query.is_empty() {
                            append_lsp_precision(
                                ctx,
                                target_path,
                                "map",
                                query,
                                precision,
                                &mut formatted,
                                &mut metadata,
                            )
                            .await;
                        }
                        return Ok(ToolResult::text(formatted).with_metadata(metadata));
                    }
                    ripwire_failure = Some(error);
                }
                Some(McpAttempt::CliOnly { stderr }) => {
                    ripwire_stderr = trimmed_stderr(&stderr);
                    // Mock scripts and CLI-only ripwire builds route through the CLI invocation.
                    match run_ripwire(ripwire_path, target_path, query, max_tokens, limit).await {
                        Ok(run) => {
                            let cli_stderr = trimmed_stderr(&run.stderr).or(ripwire_stderr.clone());
                            if !run.stdout.trim().is_empty() {
                                let mut compacted = compact_ripwire_cli_output(&run.stdout);
                                let mut metadata = json!({
                                    "operation": "map",
                                    "engine": "ripwire",
                                    "query": query,
                                    "token_budget": max_tokens,
                                });
                                if scip_for_mode(&root, precision).is_some() {
                                    metadata["scip_overlay"] = json!(true);
                                }
                                if query.is_empty() {
                                    metadata["limit"] = json!(limit);
                                }
                                if let Some(stderr) = cli_stderr {
                                    metadata["ripwire_stderr"] = json!(stderr);
                                }
                                if !query.is_empty() {
                                    append_lsp_precision(
                                        ctx,
                                        target_path,
                                        "map",
                                        query,
                                        precision,
                                        &mut compacted,
                                        &mut metadata,
                                    )
                                    .await;
                                }
                                return Ok(ToolResult::text(compacted).with_metadata(metadata));
                            }
                            ripwire_failure = Some("ripwire returned no output".to_string());
                            ripwire_stderr = cli_stderr;
                        }
                        Err(RipwireFailure::Timeout { timeout }) => {
                            return Err(timeout_error("map", timeout));
                        }
                        Err(RipwireFailure::Spawn { message }) => {
                            ripwire_failure = Some(message);
                        }
                        Err(RipwireFailure::Exit { code, stderr }) => {
                            ripwire_stderr = trimmed_stderr(&stderr);
                            ripwire_failure = Some(format!(
                                "ripwire exited with status {}",
                                code.map_or_else(|| "signal".to_string(), |c| c.to_string())
                            ));
                        }
                    }
                }
                None => {}
            }
        }

        // Cached pure-Rust graph-boosted fallback.
        let (mut out, total_files_emitted, total_symbols_emitted) =
            native_map(ctx, target_path, query, rank_by, max_tokens, limit)?;

        let mut metadata = json!({
            "operation": "map",
            "engine": "native",
            "query": query,
            "files": total_files_emitted,
            "symbols": total_symbols_emitted,
        });
        if let Some(mode) = rank_by {
            metadata["rank_by"] = json!(mode);
        }
        add_fallback_metadata(
            &mut out,
            &mut metadata,
            ripwire_binary.is_none(),
            ripwire_failure,
            ripwire_stderr,
        );
        if !query.is_empty() {
            append_lsp_precision(
                ctx,
                target_path,
                "map",
                query,
                precision,
                &mut out,
                &mut metadata,
            )
            .await;
        }
        Ok(ToolResult::text(out).with_metadata(metadata))
    }
}
