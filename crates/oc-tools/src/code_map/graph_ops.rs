//! Structural graph and multi-hop operations (`pack_task`, `communities`,
//! `symbol`, `expand`, `impact`, `references`, `connect`, `affected`, `trace`,
//! `batch`, `lego`, `slice`, `cochange`, `doc_drift`, `flags`, `quality`)
//! backed by the persistent `ripwire --mcp` pool with CLI and cached pure-Rust
//! native fallbacks plus SCIP / `oc-lsp` precision overlays.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::CodeMapTool;
use super::files::{read_bounded_file, relative_to};
use super::native_index::{
    native_affected, native_cochange, native_communities, native_connect, native_doc_drift,
    native_expand, native_flags, native_impact, native_lego, native_map, native_pack_task,
    native_quality, native_slice, native_symbol, native_trace, relative_scope, tree_root_for,
};
use super::parsers::parse_symbols;
use super::precision::{PrecisionMode, append_lsp_precision, scip_for_mode};
use super::radar::partition_task;
use super::ripwire::{
    RIPWIRE_INSTALL_CMD, RipwireFailure, compact_ripwire_cli_output, configured_ripwire_timeout,
    find_ripwire_binary, run_ripwire_args, strip_comments_and_blank_runs, trimmed_stderr,
};
use super::ripwire_mcp::{McpCallError, McpToolOutput, call_mcp_tool_with_scip};
use crate::{ToolContext, ToolError, ToolResult};

/// Outcome of attempting an operation against the persistent `ripwire --mcp` pool.
pub(super) enum McpAttempt {
    Ok(McpToolOutput),
    CliOnly { stderr: Vec<u8> },
    Fallback { error: String, stderr: Vec<u8> },
}

pub(super) fn timeout_error(operation: &str, timeout: Duration) -> ToolError {
    ToolError::new(format!(
        "ripwire did not finish within {} ms; raise OPENCODE_RIPWIRE_TIMEOUT_MS \
         or pass `query` to narrow the scan",
        timeout.as_millis()
    ))
    .with_metadata(json!({
        "operation": operation,
        "engine": "ripwire",
        "timeout_ms": timeout.as_millis(),
    }))
}

pub(super) async fn run_mcp(
    ctx: &ToolContext,
    root: &Path,
    operation: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<McpAttempt>, ToolError> {
    run_mcp_with_scip(ctx, root, None, operation, tool_name, arguments).await
}

pub(super) async fn run_mcp_with_scip(
    ctx: &ToolContext,
    root: &Path,
    scip_path: Option<&Path>,
    operation: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<McpAttempt>, ToolError> {
    if ctx
        .cancel
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(ToolError::new("Tool execution cancelled"));
    }
    let Some(binary) = find_ripwire_binary() else {
        return Ok(None);
    };
    let cancel = ctx.cancel.clone().unwrap_or_default();
    let timeout = configured_ripwire_timeout();

    match call_mcp_tool_with_scip(
        &binary, root, scip_path, &cancel, timeout, tool_name, arguments,
    )
    .await
    {
        Ok(out) => Ok(Some(McpAttempt::Ok(out))),
        Err(McpCallError::Timeout { timeout }) => Err(timeout_error(operation, timeout)),
        Err(McpCallError::Cancelled) => Err(ToolError::new("Tool execution cancelled")),
        Err(McpCallError::CliOnly { stderr }) => Ok(Some(McpAttempt::CliOnly { stderr })),
        Err(McpCallError::Tool { message, stderr } | McpCallError::Io { message, stderr }) => {
            Ok(Some(McpAttempt::Fallback {
                error: message,
                stderr,
            }))
        }
    }
}

pub(super) async fn run_mcp_mode(
    ctx: &ToolContext,
    root: &Path,
    precision: PrecisionMode,
    operation: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<(Option<McpAttempt>, bool), ToolError> {
    let scip = scip_for_mode(root, precision);
    let used_scip = scip.is_some();
    let attempt =
        run_mcp_with_scip(ctx, root, scip.as_deref(), operation, tool_name, arguments).await?;
    Ok((attempt, used_scip))
}

pub(crate) fn parse_mcp_json_text(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        let end = trimmed.rfind('}')?;
        serde_json::from_str::<Value>(&trimmed[..=end]).ok()
    })
}

fn record_scip_meta(metadata: &mut Value, precision: PrecisionMode, used_scip: bool) {
    if precision != PrecisionMode::Fast {
        metadata["precision"] = json!(precision.as_str());
    }
    if used_scip {
        metadata["scip_overlay"] = json!(true);
    }
}

pub(super) fn add_fallback_metadata(
    out: &mut String,
    metadata: &mut Value,
    binary_missing: bool,
    failure: Option<String>,
    stderr: Option<String>,
) {
    if binary_missing {
        let _ = writeln!(
            out,
            "\n[Tip: For deep call graphs and Personalized PageRank, install ripwire: `{RIPWIRE_INSTALL_CMD}`]"
        );
        metadata["ripwire_install_command"] = json!(RIPWIRE_INSTALL_CMD);
    } else if let Some(reason) = failure {
        metadata["ripwire_error"] = json!(reason);
    }
    if let Some(err) = stderr {
        metadata["ripwire_stderr"] = json!(err);
    }
}

fn matches_scope(rel_file: &str, rel_scope: Option<&str>) -> bool {
    let Some(prefix) = rel_scope else {
        return true;
    };
    let norm = rel_file.replace('\\', "/");
    norm == prefix || norm.starts_with(&format!("{prefix}/"))
}

fn mcp_metadata(operation: &str, stderr: &[u8], extra: &Value) -> Value {
    let mut metadata = json!({
        "operation": operation,
        "engine": "ripwire",
        "transport": "mcp",
    });
    if let (Some(dst), Some(src)) = (metadata.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    if let Some(s) = trimmed_stderr(stderr) {
        metadata["ripwire_stderr"] = json!(s);
    }
    metadata
}

fn build_mcp_result(operation: &str, text: String, stderr: &[u8], extra: &Value) -> ToolResult {
    ToolResult::text(text).with_metadata(mcp_metadata(operation, stderr, extra))
}

#[allow(clippy::too_many_arguments)]
async fn finalize_precision(
    ctx: &ToolContext,
    target_path: &Path,
    op: &str,
    query: &str,
    precision: PrecisionMode,
    used_scip: bool,
    mut out: String,
    mut metadata: Value,
) -> ToolResult {
    record_scip_meta(&mut metadata, precision, used_scip);
    if !query.is_empty() {
        append_lsp_precision(
            ctx,
            target_path,
            op,
            query,
            precision,
            &mut out,
            &mut metadata,
        )
        .await;
    }
    ToolResult::text(out).with_metadata(metadata)
}

impl CodeMapTool {
    pub(super) async fn execute_pack_task(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        max_tokens: usize,
        limit: usize,
        partitions: Option<usize>,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        if let Some(part_count) = partitions {
            let partitioned =
                partition_task(ctx, target_path, query, part_count, max_tokens).await?;
            let text = partitioned.format_for_tool();
            let mut metadata = json!({
                "operation": "pack_task",
                "engine": partitioned.engine,
                "query": query,
                "token_budget": max_tokens,
                "partitions": part_count,
            });
            if let (Some(dst), Some(src)) = (
                metadata.as_object_mut(),
                partitioned.metadata_summary().as_object(),
            ) {
                for (k, v) in src {
                    dst.insert(k.clone(), v.clone());
                }
            }
            return Ok(ToolResult::text(text).with_metadata(metadata));
        }

        let root = tree_root_for(ctx, target_path);
        let rel_scope = relative_scope(&root, target_path);
        let scoped_task = match &rel_scope {
            Some(rel) => format!("{query} (scope: {rel})"),
            None => query.to_string(),
        };

        let mut failure = None;
        let mut stderr = None;

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "pack_task",
            "explore",
            &json!({
                "path": root,
                "task": scoped_task,
                "budget_tokens": max_tokens,
            }),
        )
        .await?;

        if let Some(attempt) = attempt {
            match attempt {
                McpAttempt::Ok(out) if !out.text.trim().is_empty() => {
                    let compacted = compact_ripwire_cli_output(&out.text);
                    let mut res = build_mcp_result(
                        "pack_task",
                        compacted,
                        &out.stderr,
                        &json!({ "query": query, "token_budget": max_tokens }),
                    );
                    if let Some(meta) = res.metadata.as_mut() {
                        record_scip_meta(meta, precision, used_scip);
                    }
                    return Ok(res);
                }
                McpAttempt::Ok(out) => {
                    stderr = trimmed_stderr(&out.stderr);
                    failure = Some("ripwire explore returned empty output".to_string());
                }
                McpAttempt::Fallback { error, stderr: err } => {
                    stderr = trimmed_stderr(&err);
                    failure = Some(error);
                }
                McpAttempt::CliOnly { stderr: err } => {
                    stderr = trimmed_stderr(&err);
                    failure = Some("ripwire binary does not support MCP explore".to_string());
                }
            }
        }

        let (mut out, sigs) = native_pack_task(ctx, target_path, query, max_tokens, limit)?;
        let mut metadata = json!({
            "operation": "pack_task",
            "engine": "native",
            "query": query,
            "token_budget": max_tokens,
            "symbols": sigs,
        });
        add_fallback_metadata(
            &mut out,
            &mut metadata,
            find_ripwire_binary().is_none(),
            failure,
            stderr,
        );
        Ok(ToolResult::text(out).with_metadata(metadata))
    }

    pub(super) async fn execute_communities(
        ctx: &ToolContext,
        target_path: &Path,
        query: Option<&str>,
        max_tokens: usize,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let binary = find_ripwire_binary();
        let root = tree_root_for(ctx, target_path);
        let mut failure = None;
        let mut stderr_note = None;

        if let Some(bin) = binary.as_ref() {
            let mut args = vec![
                OsString::from(root.as_os_str()),
                OsString::from("--communities"),
            ];
            if let Some(q) = query.map(str::trim).filter(|q| !q.is_empty())
                && q.chars().all(|c| c.is_ascii_digit())
            {
                args.pop();
                args.push(OsString::from(format!("--community={q}")));
            }

            let cancel = ctx.cancel.clone().unwrap_or_default();
            let cli_res = tokio::select! {
                () = cancel.cancelled() => return Err(ToolError::new("Tool execution cancelled")),
                res = run_ripwire_args(bin, &args) => res,
            };

            match cli_res {
                Ok(run) => {
                    stderr_note = trimmed_stderr(&run.stderr);
                    let trimmed = run.stdout.trim();
                    if trimmed.contains("<communit") {
                        let compacted = compact_ripwire_cli_output(trimmed);
                        let budgeted = truncate_budget(&compacted, max_tokens.saturating_mul(4));
                        let mut metadata = json!({
                            "operation": "communities",
                            "engine": "ripwire",
                            "transport": "cli",
                            "token_budget": max_tokens,
                        });
                        if let Some(err) = stderr_note {
                            metadata["ripwire_stderr"] = json!(err);
                        }
                        return Ok(ToolResult::text(budgeted).with_metadata(metadata));
                    }
                    failure =
                        Some("ripwire --communities returned non-community output".to_string());
                }
                Err(RipwireFailure::Timeout { timeout }) => {
                    return Err(timeout_error("communities", timeout));
                }
                Err(RipwireFailure::Spawn { message }) => {
                    failure = Some(message);
                }
                Err(RipwireFailure::Exit { code, stderr }) => {
                    stderr_note = trimmed_stderr(&stderr);
                    failure = Some(format!(
                        "ripwire exited with status {}",
                        code.map_or_else(|| "signal".to_string(), |c| c.to_string())
                    ));
                }
            }
        }

        let (mut out, modules) = native_communities(ctx, target_path, query, limit)?;
        let mut metadata = json!({
            "operation": "communities",
            "engine": "native",
            "modules": modules,
        });
        add_fallback_metadata(
            &mut out,
            &mut metadata,
            binary.is_none(),
            failure,
            stderr_note,
        );
        Ok(ToolResult::text(out).with_metadata(metadata))
    }

    pub(super) async fn execute_symbol(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let rel_scope = relative_scope(&root, target_path);

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "symbol",
            "find_symbol",
            &json!({ "path": root, "symbol": query, "limit": limit }),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && let Some(formatted) =
                format_find_symbol_json(ctx, &root, rel_scope.as_deref(), query, &out.text)
        {
            let metadata = mcp_metadata("symbol", &out.stderr, &json!({ "query": query }));
            return Ok(finalize_precision(
                ctx,
                target_path,
                "symbol",
                query,
                precision,
                used_scip,
                formatted,
                metadata,
            )
            .await);
        }

        let (text, matches) = native_symbol(ctx, target_path, query, limit)?;
        let metadata = json!({
            "operation": "symbol",
            "engine": "native",
            "query": query,
            "matches": matches,
        });
        Ok(finalize_precision(
            ctx,
            target_path,
            "symbol",
            query,
            precision,
            false,
            text,
            metadata,
        )
        .await)
    }

    pub(super) async fn execute_expand(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        start_line: Option<usize>,
        end_line: Option<usize>,
        compress: bool,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let rel_scope = relative_scope(&root, target_path);

        let direct_handle = resolve_direct_handle(query, rel_scope.as_deref());
        let handle = if let Some(h) = direct_handle {
            Some(h)
        } else if let Some(McpAttempt::Ok(sym_out)) = run_mcp(
            ctx,
            &root,
            "expand",
            "find_symbol",
            &json!({ "path": root, "symbol": query, "limit": 1 }),
        )
        .await?
        {
            extract_handle_from_find_symbol(&sym_out.text, rel_scope.as_deref())
        } else {
            None
        };

        if let Some(h) = handle {
            let mut args = Map::new();
            args.insert("path".to_string(), json!(root));
            args.insert("handle".to_string(), json!(h));
            if let Some(s) = start_line {
                args.insert("start_line".to_string(), json!(s));
            }
            if let Some(e) = end_line {
                args.insert("end_line".to_string(), json!(e));
            }

            if let Some(McpAttempt::Ok(out)) =
                run_mcp(ctx, &root, "expand", "fetch_body", &Value::Object(args)).await?
                && let Some((formatted, file_rel)) =
                    format_fetch_body_json(ctx, &root, &out.text, compress)
            {
                return Ok(build_mcp_result(
                    "expand",
                    formatted,
                    &out.stderr,
                    &json!({ "query": query, "file": file_rel, "compress": compress }),
                ));
            }
        }

        let (text, file_rel) =
            native_expand(ctx, target_path, query, start_line, end_line, compress)?;
        Ok(ToolResult::text(text).with_metadata(json!({
            "operation": "expand",
            "engine": "native",
            "query": query,
            "file": file_rel,
            "compress": compress,
        })))
    }

    pub(super) async fn execute_impact(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "impact",
            "impact",
            &json!({ "path": root, "symbol": query, "limit": limit }),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let metadata = mcp_metadata("impact", &out.stderr, &json!({ "query": query }));
            return Ok(finalize_precision(
                ctx,
                target_path,
                "impact",
                query,
                precision,
                used_scip,
                compact_ripwire_cli_output(&out.text),
                metadata,
            )
            .await);
        }

        let (text, dependents) = native_impact(ctx, target_path, query, limit)?;
        let metadata = json!({
            "operation": "impact",
            "engine": "native",
            "query": query,
            "dependents": dependents,
        });
        Ok(finalize_precision(
            ctx,
            target_path,
            "impact",
            query,
            precision,
            false,
            text,
            metadata,
        )
        .await)
    }

    pub(super) async fn execute_references(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let rel_scope = relative_scope(&root, target_path);

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "references",
            "uses",
            &json!({ "path": root, "symbol": query, "limit": limit }),
        )
        .await?;
        if let Some(McpAttempt::Ok(uses_out)) = attempt {
            if let Some((formatted, matches)) =
                format_uses_xml(ctx, &root, rel_scope.as_deref(), query, &uses_out.text)
                && matches > 0
            {
                let metadata = mcp_metadata(
                    "references",
                    &uses_out.stderr,
                    &json!({ "query": query, "matches": matches }),
                );
                return Ok(finalize_precision(
                    ctx,
                    target_path,
                    "references",
                    query,
                    precision,
                    used_scip,
                    formatted,
                    metadata,
                )
                .await);
            }

            // Automatically fall back to trigram literal grep for string literals / non-call uses.
            if let Some(McpAttempt::Ok(grep_out)) = run_mcp(
                ctx,
                &root,
                "references",
                "grep",
                &json!({ "path": root, "pattern": query, "limit": limit, "in": "any" }),
            )
            .await?
                && let Some((formatted, matches)) = format_grep_json(
                    ctx,
                    &root,
                    rel_scope.as_deref(),
                    query,
                    &grep_out.text,
                    limit,
                )
                && matches > 0
            {
                let metadata = mcp_metadata(
                    "references",
                    &grep_out.stderr,
                    &json!({ "query": query, "matches": matches }),
                );
                return Ok(finalize_precision(
                    ctx,
                    target_path,
                    "references",
                    query,
                    precision,
                    used_scip,
                    formatted,
                    metadata,
                )
                .await);
            }
        }

        let res = Self::execute_references_native(ctx, target_path, query, limit)?;
        Ok(finalize_precision(
            ctx,
            target_path,
            "references",
            query,
            precision,
            false,
            res.output.unwrap_or_default(),
            res.metadata
                .unwrap_or_else(|| json!({ "operation": "references" })),
        )
        .await)
    }

    pub(super) async fn execute_connect(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let (from, to) = parse_connect_endpoints(query).ok_or_else(|| {
            ToolError::new(
                "Operation 'connect' requires two symbol endpoints in `query`, e.g. `A -> B`",
            )
        })?;

        let root = tree_root_for(ctx, target_path);
        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "connect",
            "connect",
            &json!({ "path": root, "symbols": [from, to] }),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let metadata = mcp_metadata("connect", &out.stderr, &json!({ "from": from, "to": to }));
            return Ok(finalize_precision(
                ctx,
                target_path,
                "connect",
                &from,
                precision,
                used_scip,
                compact_ripwire_cli_output(&out.text),
                metadata,
            )
            .await);
        }

        let (text, hops) = native_connect(ctx, target_path, &from, &to)?;
        let metadata = json!({
            "operation": "connect",
            "engine": "native",
            "from": from,
            "to": to,
            "hops": hops,
        });
        Ok(finalize_precision(
            ctx,
            target_path,
            "connect",
            &from,
            precision,
            false,
            text,
            metadata,
        )
        .await)
    }

    pub(super) async fn execute_affected(
        ctx: &ToolContext,
        target_path: &Path,
        query: Option<&str>,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let seed_csv = match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => q.to_string(),
            None => detect_git_changed_csv(&root).await,
        };

        if seed_csv.is_empty() {
            return Ok(ToolResult::text(
                "No modified git files detected; pass comma-separated changed file paths or symbols in `query`.",
            )
            .with_metadata(json!({
                "operation": "affected",
                "engine": "native",
                "tests": 0,
            })));
        }

        if let Some(McpAttempt::Ok(out)) = run_mcp(
            ctx,
            &root,
            "affected",
            "affected",
            &json!({ "path": root, "files": seed_csv }),
        )
        .await?
            && !out.text.trim().is_empty()
        {
            return Ok(build_mcp_result(
                "affected",
                compact_ripwire_cli_output(&out.text),
                &out.stderr,
                &json!({ "seeds": seed_csv }),
            ));
        }

        let (text, tests) = native_affected(ctx, target_path, &seed_csv, limit)?;
        Ok(ToolResult::text(text).with_metadata(json!({
            "operation": "affected",
            "engine": "native",
            "seeds": seed_csv,
            "tests": tests,
        })))
    }

    pub(super) async fn execute_trace(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        max_tokens: usize,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        if let Some(McpAttempt::Ok(out)) = run_mcp(
            ctx,
            &root,
            "trace",
            "from_trace",
            &json!({ "path": root, "trace": query, "budget_tokens": max_tokens }),
        )
        .await?
            && !out.text.trim().is_empty()
        {
            return Ok(build_mcp_result(
                "trace",
                compact_ripwire_cli_output(&out.text),
                &out.stderr,
                &json!({ "token_budget": max_tokens }),
            ));
        }

        let (text, resolved) = native_trace(ctx, target_path, query, max_tokens)?;
        Ok(ToolResult::text(text).with_metadata(json!({
            "operation": "trace",
            "engine": "native",
            "resolved": resolved,
        })))
    }

    pub(super) async fn execute_batch(
        ctx: &ToolContext,
        target_path: &Path,
        queries: &[String],
        max_tokens: usize,
        limit: usize,
        compress: bool,
    ) -> Result<ToolResult, ToolError> {
        if queries.is_empty() || queries.len() > 16 {
            return Err(ToolError::new(
                "`queries` must contain between 1 and 16 entries for operation 'batch'",
            ));
        }

        let parsed: Vec<BatchItem> = queries
            .iter()
            .map(|raw| parse_batch_item(raw))
            .collect::<Result<Vec<_>, _>>()?;

        let root = tree_root_for(ctx, target_path);
        if parsed.iter().all(BatchItem::supports_mcp_batch) {
            let mcp_queries: Vec<Value> =
                parsed.iter().map(|item| item.to_mcp_json(limit)).collect();
            if let Some(McpAttempt::Ok(out)) = run_mcp(
                ctx,
                &root,
                "batch",
                "batch",
                &json!({ "path": root, "queries": mcp_queries }),
            )
            .await?
                && !out.text.trim().is_empty()
            {
                let compacted = compact_ripwire_cli_output(&out.text);
                return Ok(build_mcp_result(
                    "batch",
                    compacted,
                    &out.stderr,
                    &json!({ "count": queries.len() }),
                ));
            }
        }

        // Native (or mixed) batched fallback: evaluate each sub-query locally.
        let mut combined = String::from("<batch>\n");
        let per_budget = (max_tokens / queries.len().max(1)).max(250);
        for (idx, item) in parsed.iter().enumerate() {
            let section = match item.verb.as_str() {
                "find_symbol" | "symbol" => {
                    native_symbol(ctx, target_path, &item.arg, limit).map(|(s, _)| s)
                }
                "impact" => native_impact(ctx, target_path, &item.arg, limit).map(|(s, _)| s),
                "uses" | "references" => {
                    Self::execute_references_native(ctx, target_path, &item.arg, limit)
                        .map(|r| r.output.unwrap_or_default())
                }
                "for" | "map" => native_map(ctx, target_path, &item.arg, None, per_budget, limit)
                    .map(|(s, _, _)| s),
                "explore" | "pack_task" => {
                    native_pack_task(ctx, target_path, &item.arg, per_budget, limit).map(|(s, _)| s)
                }
                "expand" | "fetch_body" => {
                    native_expand(ctx, target_path, &item.arg, None, None, compress).map(|(s, _)| s)
                }
                "connect" | "path_between" => {
                    let (a, b) = parse_connect_endpoints(&item.arg).ok_or_else(|| {
                        ToolError::new(format!("Invalid connect pair in batch: '{}'", item.arg))
                    })?;
                    native_connect(ctx, target_path, &a, &b).map(|(s, _)| s)
                }
                "affected" => {
                    let csv = if item.arg.is_empty() {
                        detect_git_changed_csv(&root).await
                    } else {
                        item.arg.clone()
                    };
                    native_affected(ctx, target_path, &csv, limit).map(|(s, _)| s)
                }
                "trace" | "from_trace" => {
                    native_trace(ctx, target_path, &item.arg, per_budget).map(|(s, _)| s)
                }
                "communities" => {
                    let filter = (!item.arg.is_empty()).then_some(item.arg.as_str());
                    native_communities(ctx, target_path, filter, limit).map(|(s, _)| s)
                }
                "grep" => Self::execute_references_native(ctx, target_path, &item.arg, limit)
                    .map(|r| r.output.unwrap_or_default()),
                other => Err(ToolError::new(format!("Unsupported batch verb '{other}'"))),
            }?;
            let _ = writeln!(
                combined,
                "<q i=\"{idx}\" verb=\"{}\">\n{}\n</q>",
                item.verb, section
            );
        }
        combined.push_str("</batch>");

        Ok(ToolResult::text(combined).with_metadata(json!({
            "operation": "batch",
            "engine": "native",
            "count": queries.len(),
        })))
    }

    pub(super) async fn execute_lego(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "lego",
            "lego",
            &json!({ "path": root, "type": query, "limit": limit }),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let metadata = mcp_metadata("lego", &out.stderr, &json!({ "query": query }));
            return Ok(finalize_precision(
                ctx,
                target_path,
                "lego",
                query,
                precision,
                used_scip,
                compact_ripwire_cli_output(&out.text),
                metadata,
            )
            .await);
        }

        let (text, implementors) = native_lego(ctx, target_path, query, limit)?;
        let metadata = json!({
            "operation": "lego",
            "engine": "native",
            "query": query,
            "implementors": implementors,
        });
        Ok(finalize_precision(
            ctx,
            target_path,
            "lego",
            query,
            precision,
            false,
            text,
            metadata,
        )
        .await)
    }

    pub(super) async fn execute_slice(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        var: Option<&str>,
        flow: Option<&str>,
        depth: Option<usize>,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let mut args = Map::new();
        args.insert("path".to_string(), json!(root));
        args.insert("symbol".to_string(), json!(query));
        if let Some(v) = var.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert("var".to_string(), json!(v));
        }
        if let Some(f) = flow.map(str::trim).filter(|s| !s.is_empty()) {
            args.insert("flow".to_string(), json!(f));
        }
        if let Some(d) = depth {
            args.insert("depth".to_string(), json!(d.clamp(1, 32)));
        }

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "slice",
            "slice",
            &Value::Object(args),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let mut extra = json!({ "query": query });
            if let Some(v) = var {
                extra["var"] = json!(v);
            }
            if let Some(f) = flow {
                extra["flow"] = json!(f);
            }
            let metadata = mcp_metadata("slice", &out.stderr, &extra);
            return Ok(finalize_precision(
                ctx,
                target_path,
                "slice",
                query,
                precision,
                used_scip,
                compact_ripwire_cli_output(&out.text),
                metadata,
            )
            .await);
        }

        let (text, hits) = native_slice(ctx, target_path, query, var, flow, depth.unwrap_or(8))?;
        let mut metadata = json!({
            "operation": "slice",
            "engine": "native",
            "query": query,
            "hits": hits,
        });
        if let Some(v) = var {
            metadata["var"] = json!(v);
        }
        if let Some(f) = flow {
            metadata["flow"] = json!(f);
        }
        Ok(finalize_precision(
            ctx,
            target_path,
            "slice",
            query,
            precision,
            false,
            text,
            metadata,
        )
        .await)
    }

    pub(super) async fn execute_cochange(
        ctx: &ToolContext,
        target_path: &Path,
        query: Option<&str>,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let q_file = query.map(str::trim).filter(|s| !s.is_empty());
        let (mcp_file, native_target): (String, PathBuf) = if let Some(q) = q_file {
            let candidate = ctx.directory.join(q);
            let resolved = if candidate.exists() {
                candidate
            } else {
                root.join(q)
            };
            (q.to_string(), resolved)
        } else if target_path.is_file() {
            (relative_to(&root, target_path), target_path.to_path_buf())
        } else {
            return Err(ToolError::new(
                "Operation 'cochange' requires a target file via `path` or `query`.",
            ));
        };

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "cochange",
            "cochange",
            &json!({ "path": root, "file": mcp_file, "limit": limit }),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let mut metadata = mcp_metadata("cochange", &out.stderr, &json!({ "file": mcp_file }));
            record_scip_meta(&mut metadata, precision, used_scip);
            return Ok(
                ToolResult::text(compact_ripwire_cli_output(&out.text)).with_metadata(metadata)
            );
        }

        let (text, partners) = native_cochange(ctx, &native_target, limit);
        let mut metadata = json!({
            "operation": "cochange",
            "engine": "native",
            "file": mcp_file,
            "partners": partners,
        });
        record_scip_meta(&mut metadata, precision, false);
        Ok(ToolResult::text(text).with_metadata(metadata))
    }

    pub(super) async fn execute_doc_drift(
        ctx: &ToolContext,
        target_path: &Path,
        query: Option<&str>,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let filter = query.map(str::trim).filter(|s| !s.is_empty());
        let mut args = Map::new();
        args.insert("path".to_string(), json!(root));
        args.insert("limit".to_string(), json!(limit));
        if let Some(k) = filter {
            args.insert("kind".to_string(), json!(k));
        }

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "doc_drift",
            "doc_drift",
            &Value::Object(args),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let mut metadata = mcp_metadata("doc_drift", &out.stderr, &json!({}));
            record_scip_meta(&mut metadata, precision, used_scip);
            return Ok(
                ToolResult::text(compact_ripwire_cli_output(&out.text)).with_metadata(metadata)
            );
        }

        let (text, findings) = native_doc_drift(ctx, target_path, filter, limit)?;
        let mut metadata = json!({
            "operation": "doc_drift",
            "engine": "native",
            "findings": findings,
        });
        record_scip_meta(&mut metadata, precision, false);
        Ok(ToolResult::text(text).with_metadata(metadata))
    }

    pub(super) async fn execute_flags(
        ctx: &ToolContext,
        target_path: &Path,
        query: Option<&str>,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let root = tree_root_for(ctx, target_path);
        let filter = query.map(str::trim).filter(|s| !s.is_empty());
        let mut args = Map::new();
        args.insert("path".to_string(), json!(root));
        args.insert("limit".to_string(), json!(limit));
        if let Some(k) = filter {
            args.insert("kind".to_string(), json!(k));
        }

        let (attempt, used_scip) = run_mcp_mode(
            ctx,
            &root,
            precision,
            "flags",
            "flags",
            &Value::Object(args),
        )
        .await?;
        if let Some(McpAttempt::Ok(out)) = attempt
            && !out.text.trim().is_empty()
        {
            let mut metadata = mcp_metadata("flags", &out.stderr, &json!({}));
            record_scip_meta(&mut metadata, precision, used_scip);
            return Ok(
                ToolResult::text(compact_ripwire_cli_output(&out.text)).with_metadata(metadata)
            );
        }

        let (text, gates) = native_flags(ctx, target_path, filter, limit)?;
        let mut metadata = json!({
            "operation": "flags",
            "engine": "native",
            "gates": gates,
        });
        record_scip_meta(&mut metadata, precision, false);
        Ok(ToolResult::text(text).with_metadata(metadata))
    }

    pub(super) async fn execute_quality(
        ctx: &ToolContext,
        target_path: &Path,
        mode_query: Option<&str>,
        max_tokens: usize,
        limit: usize,
        precision: PrecisionMode,
    ) -> Result<ToolResult, ToolError> {
        let mode = mode_query
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("delta");
        let flag = match mode {
            "delta" => None,
            "dead_code" => Some("--dead-code"),
            "clones" => Some("--clones"),
            "hotspots" => Some("--hotspots"),
            "deps" => Some("--deps"),
            "seams" => Some("--seams"),
            other => {
                return Err(ToolError::new(format!(
                    "Invalid quality lens '{other}'. Expected 'delta', 'dead_code', \
                     'clones', 'hotspots', 'deps', or 'seams'."
                )));
            }
        };

        let root = tree_root_for(ctx, target_path);
        let binary = find_ripwire_binary();
        let (hit, failure, stderr_note) = if let Some(cli_flag) = flag {
            try_quality_cli(
                ctx,
                binary.as_deref(),
                &root,
                cli_flag,
                mode,
                max_tokens,
                precision,
            )
            .await?
        } else {
            try_quality_delta_mcp(ctx, &root, mode, max_tokens, precision).await?
        };
        if let Some(res) = hit {
            return Ok(res);
        }

        let (mut out, findings) = native_quality(ctx, target_path, mode, limit)?;
        let mut metadata = json!({
            "operation": "quality",
            "mode": mode,
            "engine": "native",
            "findings": findings,
        });
        record_scip_meta(&mut metadata, precision, false);
        add_fallback_metadata(
            &mut out,
            &mut metadata,
            binary.is_none(),
            failure,
            stderr_note,
        );
        Ok(ToolResult::text(out).with_metadata(metadata))
    }
}

async fn try_quality_cli(
    ctx: &ToolContext,
    binary: Option<&Path>,
    root: &Path,
    cli_flag: &str,
    mode: &str,
    max_tokens: usize,
    precision: PrecisionMode,
) -> Result<(Option<ToolResult>, Option<String>, Option<String>), ToolError> {
    let Some(bin) = binary else {
        return Ok((None, None, None));
    };
    let mut args = vec![OsString::from(root.as_os_str()), OsString::from(cli_flag)];
    let scip = scip_for_mode(root, precision);
    if let Some(scip_path) = scip.as_ref() {
        args.push(OsString::from(format!(
            "--scip={}",
            scip_path.to_string_lossy()
        )));
    }
    let cancel = ctx.cancel.clone().unwrap_or_default();
    let cli_res = tokio::select! {
        () = cancel.cancelled() => return Err(ToolError::new("Tool execution cancelled")),
        res = run_ripwire_args(bin, &args) => res,
    };
    match cli_res {
        Ok(run) => {
            let stderr_note = trimmed_stderr(&run.stderr);
            let trimmed = run.stdout.trim();
            if !trimmed.is_empty() {
                let compacted = compact_ripwire_cli_output(trimmed);
                let budgeted = truncate_budget(&compacted, max_tokens.saturating_mul(4));
                let mut metadata = json!({
                    "operation": "quality",
                    "mode": mode,
                    "engine": "ripwire",
                    "transport": "cli",
                    "token_budget": max_tokens,
                });
                record_scip_meta(&mut metadata, precision, scip.is_some());
                if let Some(err) = stderr_note {
                    metadata["ripwire_stderr"] = json!(err);
                }
                return Ok((
                    Some(ToolResult::text(budgeted).with_metadata(metadata)),
                    None,
                    None,
                ));
            }
            Ok((
                None,
                Some(format!("ripwire {cli_flag} returned empty output")),
                stderr_note,
            ))
        }
        Err(RipwireFailure::Timeout { timeout }) => Err(timeout_error("quality", timeout)),
        Err(RipwireFailure::Spawn { message }) => Ok((None, Some(message), None)),
        Err(RipwireFailure::Exit { code, stderr }) => Ok((
            None,
            Some(format!(
                "ripwire exited with status {}",
                code.map_or_else(|| "signal".to_string(), |c| c.to_string())
            )),
            trimmed_stderr(&stderr),
        )),
    }
}

async fn try_quality_delta_mcp(
    ctx: &ToolContext,
    root: &Path,
    mode: &str,
    max_tokens: usize,
    precision: PrecisionMode,
) -> Result<(Option<ToolResult>, Option<String>, Option<String>), ToolError> {
    let (attempt, used_scip) = run_mcp_mode(
        ctx,
        root,
        precision,
        "quality",
        "quality_delta",
        &json!({ "path": root }),
    )
    .await?;
    match attempt {
        Some(McpAttempt::Ok(out)) if !out.text.trim().is_empty() => {
            let compacted = compact_ripwire_cli_output(&out.text);
            let budgeted = truncate_budget(&compacted, max_tokens.saturating_mul(4));
            let mut metadata = mcp_metadata("quality", &out.stderr, &json!({ "mode": mode }));
            record_scip_meta(&mut metadata, precision, used_scip);
            Ok((
                Some(ToolResult::text(budgeted).with_metadata(metadata)),
                None,
                None,
            ))
        }
        Some(McpAttempt::Ok(out)) => Ok((
            None,
            Some("ripwire quality_delta returned empty output".to_string()),
            trimmed_stderr(&out.stderr),
        )),
        Some(McpAttempt::Fallback { error, stderr }) => {
            Ok((None, Some(error), trimmed_stderr(&stderr)))
        }
        Some(McpAttempt::CliOnly { stderr }) => Ok((
            None,
            Some("ripwire binary does not support MCP quality_delta".to_string()),
            trimmed_stderr(&stderr),
        )),
        None => Ok((None, None, None)),
    }
}

struct BatchItem {
    verb: String,
    arg: String,
}

impl BatchItem {
    fn supports_mcp_batch(&self) -> bool {
        matches!(
            self.verb.as_str(),
            "find_symbol"
                | "symbol"
                | "uses"
                | "references"
                | "impact"
                | "grep"
                | "for"
                | "map"
                | "explore"
                | "pack_task"
                | "connect"
                | "path_between"
                | "fetch_body"
                | "expand"
                | "from_trace"
                | "trace"
                | "affected"
        )
    }

    fn to_mcp_json(&self, limit: usize) -> Value {
        match self.verb.as_str() {
            "find_symbol" | "symbol" => {
                json!({ "verb": "find_symbol", "symbol": self.arg, "limit": limit })
            }
            "uses" | "references" => {
                json!({ "verb": "uses", "symbol": self.arg, "limit": limit })
            }
            "impact" => json!({ "verb": "impact", "symbol": self.arg, "limit": limit }),
            "grep" => json!({ "verb": "grep", "pattern": self.arg, "limit": limit, "in": "any" }),
            "for" | "map" => json!({ "verb": "for", "task": self.arg, "limit": limit }),
            "explore" | "pack_task" => {
                json!({ "verb": "explore", "task": self.arg, "limit": limit })
            }
            "fetch_body" | "expand" => {
                let handle =
                    resolve_direct_handle(&self.arg, None).unwrap_or_else(|| self.arg.clone());
                json!({ "verb": "fetch_body", "handle": handle })
            }
            "from_trace" | "trace" => json!({ "verb": "from_trace", "trace": self.arg }),
            "affected" => json!({ "verb": "affected", "file": self.arg }),
            "connect" | "path_between" => {
                let (from, to) = parse_connect_endpoints(&self.arg)
                    .unwrap_or_else(|| (self.arg.clone(), String::new()));
                json!({ "verb": "path_between", "from": from, "to": to })
            }
            _ => json!({ "verb": self.verb, "symbol": self.arg }),
        }
    }
}

fn parse_batch_item(raw: &str) -> Result<BatchItem, ToolError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ToolError::new("Empty sub-query in `batch` `queries`"));
    }
    if let Some((verb, arg)) = trimmed.split_once(':') {
        return Ok(BatchItem {
            verb: verb.trim().to_lowercase(),
            arg: arg.trim().to_string(),
        });
    }
    Ok(BatchItem {
        verb: trimmed.to_lowercase(),
        arg: String::new(),
    })
}

fn parse_connect_endpoints(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    for sep in ["->", "..", ","] {
        if let Some((a, b)) = trimmed.split_once(sep) {
            let left = a.trim().to_string();
            let right = b.trim().to_string();
            if !left.is_empty() && !right.is_empty() {
                return Some((left, right));
            }
        }
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() == 2 {
        return Some((parts[0].to_string(), parts[1].to_string()));
    }
    None
}

fn resolve_direct_handle(query: &str, rel_scope: Option<&str>) -> Option<String> {
    let q = query.trim();
    if q.starts_with("sym#") || q.starts_with('@') {
        return Some(q.to_string());
    }
    if let Some((file_part, rest)) = q.rsplit_once(':')
        && rest.trim().parse::<usize>().is_ok()
    {
        let file = match rel_scope {
            Some(prefix) if !file_part.contains('/') && !prefix.contains('.') => {
                format!("{prefix}/{}", file_part.trim())
            }
            _ => file_part.trim().to_string(),
        };
        return Some(format!("@{file}:{}", rest.trim()));
    }
    None
}

fn extract_handle_from_find_symbol(json_text: &str, rel_scope: Option<&str>) -> Option<String> {
    let val: Value = serde_json::from_str(json_text).ok()?;
    let sym = val.get("symbol")?;
    let file = sym.get("file").and_then(Value::as_str)?;
    if !matches_scope(file, rel_scope) {
        return None;
    }
    sym.get("handle")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn format_find_symbol_json(
    ctx: &ToolContext,
    root: &Path,
    rel_scope: Option<&str>,
    query: &str,
    json_text: &str,
) -> Option<String> {
    let val: Value = serde_json::from_str(json_text).ok()?;
    let sym = val.get("symbol")?;
    let rel_file = sym.get("file").and_then(Value::as_str)?;
    if !matches_scope(rel_file, rel_scope) {
        return None;
    }
    let name = sym.get("name").and_then(Value::as_str).unwrap_or(query);
    let raw_kind = sym.get("kind").and_then(Value::as_str).unwrap_or("symbol");
    let line = usize::try_from(sym.get("line").and_then(Value::as_u64).unwrap_or(1)).unwrap_or(1);
    let handle = sym.get("handle").and_then(Value::as_str).unwrap_or("");

    let abs_file = root.join(rel_file);
    let display_rel = relative_to(&ctx.directory, &abs_file);

    let (norm_kind, signature) = resolve_kind_and_signature(&abs_file, name, raw_kind, line);

    let mut out = format!("{display_rel}:{line} [{norm_kind}] {signature}");
    if !handle.is_empty() {
        let _ = write!(out, " | handle={handle}");
    }

    if let Some(callers) = val.get("calledBy").and_then(Value::as_array)
        && !callers.is_empty()
    {
        let names: Vec<&str> = callers
            .iter()
            .take(5)
            .filter_map(|c| c.get("name").and_then(Value::as_str))
            .collect();
        if !names.is_empty() {
            let _ = write!(out, " | callers:[{}]", names.join(","));
        }
    }
    if let Some(calls) = val.get("calls").and_then(Value::as_array)
        && !calls.is_empty()
    {
        let names: Vec<&str> = calls
            .iter()
            .take(5)
            .filter_map(|c| c.get("name").and_then(Value::as_str))
            .collect();
        if !names.is_empty() {
            let _ = write!(out, " | calls:[{}]", names.join(","));
        }
    }
    Some(out)
}

fn resolve_kind_and_signature(
    abs_file: &Path,
    name: &str,
    raw_kind: &str,
    line: usize,
) -> (String, String) {
    if let Ok(content) = read_bounded_file(abs_file) {
        let ext = abs_file.extension().and_then(|e| e.to_str()).unwrap_or("");
        let parsed = parse_symbols(&content, ext);
        if let Some(sym) = parsed
            .iter()
            .find(|s| s.name == name && s.line.abs_diff(line) <= 2)
        {
            return (sym.kind.as_str().to_string(), sym.signature.clone());
        }
        if let Some(src_line) = content.lines().nth(line.saturating_sub(1)) {
            let normalized = match raw_kind {
                "iface" => "interface",
                "cls" if src_line.contains("struct ") => "struct",
                "cls" if src_line.contains("enum ") => "enum",
                "cls" if src_line.contains("trait ") => "trait",
                "cls" => "class",
                other => other,
            };
            return (normalized.to_string(), src_line.trim().to_string());
        }
    }
    let normalized = match raw_kind {
        "iface" => "interface",
        "cls" => "class",
        other => other,
    };
    (normalized.to_string(), name.to_string())
}

fn format_fetch_body_json(
    ctx: &ToolContext,
    root: &Path,
    json_text: &str,
    compress: bool,
) -> Option<(String, String)> {
    let val: Value = serde_json::from_str(json_text).ok()?;
    let rel_file = val.get("file").and_then(Value::as_str)?;
    let name = val.get("name").and_then(Value::as_str).unwrap_or("symbol");
    let kind = val.get("kind").and_then(Value::as_str).unwrap_or("symbol");
    let def_line = val.get("line").and_then(Value::as_u64).unwrap_or(1);
    let start_off = val.get("start_line").and_then(Value::as_u64).unwrap_or(1);
    let end_off = val
        .get("end_line")
        .and_then(Value::as_u64)
        .unwrap_or(start_off);
    let raw_body = val.get("body").and_then(Value::as_str)?;

    let body = if compress {
        strip_comments_and_blank_runs(raw_body)
    } else {
        raw_body.to_string()
    };

    let abs_file = root.join(rel_file);
    let display_rel = relative_to(&ctx.directory, &abs_file);
    let abs_start = def_line.saturating_add(start_off.saturating_sub(1));
    let abs_end = def_line.saturating_add(end_off.saturating_sub(1));
    let ext = abs_file.extension().and_then(|e| e.to_str()).unwrap_or("");

    let text = format!(
        "Symbol: {name} [{kind}] ({display_rel}:{abs_start}..{abs_end})\n```{ext}\n{body}\n```"
    );
    Some((text, display_rel))
}

fn format_uses_xml(
    ctx: &ToolContext,
    root: &Path,
    rel_scope: Option<&str>,
    query: &str,
    xml_text: &str,
) -> Option<(String, usize)> {
    let mut rows = Vec::new();
    let mut rest = xml_text;
    while let Some(pos) = rest.find("<u ") {
        rest = &rest[pos..];
        let Some(end) = rest.find("/>") else {
            break;
        };
        let tag = &rest[..end + 2];
        rest = &rest[end + 2..];

        if let Some(p_val) = extract_xml_attr(tag, "p") {
            let file_only = p_val.rsplit_once(':').map_or(p_val, |(f, _)| f);
            if !matches_scope(file_only, rel_scope) {
                continue;
            }
            let role = extract_xml_attr(tag, "role").unwrap_or("use");
            let in_id = extract_xml_attr(tag, "in_id").unwrap_or("");
            let line_part = p_val.rsplit_once(':').map_or("", |(_, l)| l);
            let display_rel = relative_to(&ctx.directory, &root.join(file_only));
            rows.push(format!(
                "  {display_rel}:{line_part:<4} [{role}] in {in_id}"
            ));
        }
    }

    if rows.is_empty() {
        return None;
    }
    let total = rows.len();
    let out = format!(
        "References to \"{query}\" ({total} hits):\n{}",
        rows.join("\n")
    );
    Some((out, total))
}

fn format_grep_json(
    ctx: &ToolContext,
    root: &Path,
    rel_scope: Option<&str>,
    query: &str,
    json_text: &str,
    limit: usize,
) -> Option<(String, usize)> {
    let val: Value = serde_json::from_str(json_text).ok()?;
    let hits = val.get("hits").and_then(Value::as_array)?;
    let mut rows = Vec::new();
    for hit in hits {
        let Some(file) = hit.get("file").and_then(Value::as_str) else {
            continue;
        };
        if !matches_scope(file, rel_scope) {
            continue;
        }
        let line = hit.get("line").and_then(Value::as_u64).unwrap_or(1);
        let in_sym = hit.get("in").and_then(Value::as_str).unwrap_or("");
        let text = hit.get("text").and_then(Value::as_str).unwrap_or("").trim();
        let display_rel = relative_to(&ctx.directory, &root.join(file));
        if in_sym.is_empty() {
            rows.push(format!("  {display_rel}:{line:<4} {text}"));
        } else {
            rows.push(format!("  {display_rel}:{line:<4} [{in_sym}] {text}"));
        }
        if rows.len() >= limit {
            break;
        }
    }
    if rows.is_empty() {
        return None;
    }
    let total = rows.len();
    let out = format!(
        "References to \"{query}\" ({total} hits):\n{}",
        rows.join("\n")
    );
    Some((out, total))
}

pub(super) fn extract_xml_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(&tag[start..start + end])
}

fn truncate_budget(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut end = max_chars.max(200);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = text[..end].rfind('\n')
        && nl > max_chars / 2
    {
        end = nl;
    }
    format!("{}\n<!-- truncated to {max_chars} chars -->", &text[..end])
}

async fn detect_git_changed_csv(root: &Path) -> String {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .await
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = stdout
        .lines()
        .filter_map(|line| line.get(3..))
        .map(|p| {
            let trimmed = p.trim();
            trimmed
                .split_once(" -> ")
                .map_or(trimmed, |(_, new_p)| new_p.trim())
                .trim_matches('"')
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .collect();
    paths.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_endpoints_and_batch_items() {
        assert_eq!(
            parse_connect_endpoints("execute_map -> run_ripwire"),
            Some(("execute_map".to_string(), "run_ripwire".to_string()))
        );
        assert_eq!(
            parse_connect_endpoints("Alpha..Beta"),
            Some(("Alpha".to_string(), "Beta".to_string()))
        );
        assert_eq!(
            parse_connect_endpoints("Alpha Beta"),
            Some(("Alpha".to_string(), "Beta".to_string()))
        );
        assert_eq!(parse_connect_endpoints("only_one"), None);

        let item = parse_batch_item("find_symbol:CodeMapTool").unwrap();
        assert_eq!(item.verb, "find_symbol");
        assert_eq!(item.arg, "CodeMapTool");
        assert!(item.supports_mcp_batch());
    }
}
