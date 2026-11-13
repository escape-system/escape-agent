//! `symbol_edit` tool: symbol-level atomic edits (`replace`, `insert_before`,
//! `insert_after`, `preview`) backed by `ripwire --mcp` (`replace_symbol_body`,
//! `insert_before_symbol`, `insert_after_symbol`, `edit_check`) with a cached
//! pure-Rust AST-span fallback, BOM/EOL preservation, and unified diff receipts.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::code_map::{
    McpCallError, NativeSymbolLocation, call_mcp_tool_with_scip, configured_ripwire_timeout,
    find_native_symbol_locations, find_ripwire_binary, parse_mcp_json_text, relative_to,
    tree_root_for,
};
use crate::read::MAX_FILE_BYTES;
use crate::{Tool, ToolContext, ToolError, ToolResult, diff, util};

#[derive(Debug, Default, Clone, Copy)]
pub struct SymbolEditTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolOp {
    Replace,
    InsertBefore,
    InsertAfter,
    Preview,
}

impl SymbolOp {
    fn parse(raw: &str) -> Result<Self, ToolError> {
        match raw.trim() {
            "replace" => Ok(Self::Replace),
            "insert_before" => Ok(Self::InsertBefore),
            "insert_after" => Ok(Self::InsertAfter),
            "preview" => Ok(Self::Preview),
            other => Err(ToolError::new(format!(
                "Invalid `operation` '{other}'. Expected 'replace', 'insert_before', 'insert_after', or 'preview'."
            ))),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::InsertBefore => "insert_before",
            Self::InsertAfter => "insert_after",
            Self::Preview => "preview",
        }
    }

    const fn mcp_tool(self) -> &'static str {
        match self {
            Self::Replace => "replace_symbol_body",
            Self::InsertBefore => "insert_before_symbol",
            Self::InsertAfter => "insert_after_symbol",
            Self::Preview => "edit_check",
        }
    }
}

#[async_trait]
impl Tool for SymbolEditTool {
    fn name(&self) -> &str {
        "symbol_edit"
    }

    fn description(&self) -> &str {
        "Atomically replace or insert code anchored to a structural symbol (`SYM`, `file:SYM`, \
         or `@FILE:LINE`), or preview contract impact before writing (`operation=\"preview\"`). \
         Eliminates fragile line-number/whitespace matching and returns unified diff + \
         caller/test checks in a single call. Uses `ripwire --mcp` when installed with a \
         pure-Rust AST-span fallback."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["replace", "insert_before", "insert_after", "preview"],
                    "description": "Edit mode: 'replace' replaces the full symbol definition, \
                                    'insert_before' inserts immediately before the symbol, \
                                    'insert_after' inserts immediately after the symbol, \
                                    'preview' checks contract impact/span without modifying files."
                },
                "symbol": {
                    "type": "string",
                    "description": "Target symbol: `name`, `file:name`, or `@file:line`."
                },
                "path": {
                    "type": "string",
                    "description": "Optional file or directory path to disambiguate `symbol`."
                },
                "content": {
                    "type": "string",
                    "description": "Replacement body or text to insert (required for replace/insert_before/insert_after; optional hypothetical body for preview)."
                },
                "post_check": {
                    "type": "boolean",
                    "description": "Run ripwire post-edit contract & impacted-test checks (default: true)."
                }
            },
            "required": ["operation", "symbol"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let op = SymbolOp::parse(&util::required_string(&input, "operation")?)?;
        let symbol = util::required_string(&input, "symbol")?.trim().to_string();
        if symbol.is_empty() {
            return Err(ToolError::new("`symbol` must not be empty"));
        }
        let path_arg = util::optional_string(&input, "path")?
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        let content = util::optional_string(&input, "content")?;
        let post_check = util::optional_bool(&input, "post_check")?.unwrap_or(true);

        let body_text = match op {
            SymbolOp::Preview => content,
            SymbolOp::Replace => {
                let text = content.ok_or_else(|| {
                    ToolError::new("Parameter `content` is required for operation 'replace'")
                })?;
                if text.trim().is_empty() {
                    return Err(ToolError::new("`content` must not be empty for `replace`"));
                }
                Some(text)
            }
            SymbolOp::InsertBefore | SymbolOp::InsertAfter => {
                let text = content.ok_or_else(|| {
                    ToolError::new(format!(
                        "Parameter `content` is required for operation '{}'",
                        op.as_str()
                    ))
                })?;
                if text.is_empty() {
                    return Err(ToolError::new(format!(
                        "`content` must not be empty for '{}'",
                        op.as_str()
                    )));
                }
                Some(text)
            }
        };

        let scope_path = match path_arg.as_deref() {
            Some(p) => util::resolve(ctx, p),
            None => ctx.directory.clone(),
        };
        if path_arg.is_some() && !scope_path.exists() {
            return Err(ToolError::new(format!(
                "Path does not exist: {}",
                scope_path.display()
            )));
        }

        let root = tree_root_for(ctx, &scope_path);

        if let Some(result) = try_ripwire_symbol_edit(
            ctx,
            &root,
            &scope_path,
            op,
            &symbol,
            path_arg.as_deref(),
            body_text.as_deref(),
            post_check,
        )
        .await?
        {
            return Ok(result);
        }

        execute_native_symbol_edit(
            ctx,
            &root,
            &scope_path,
            op,
            &symbol,
            path_arg.as_deref(),
            body_text.as_deref(),
        )
        .await
    }
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    abs_path: PathBuf,
    display_rel: String,
    bom: bool,
    crlf: bool,
    clean_lf: String,
}

fn read_snapshot(base_dir: &Path, abs_path: &Path) -> Result<FileSnapshot, ToolError> {
    if let Ok(meta) = fs::metadata(abs_path)
        && meta.len() > MAX_FILE_BYTES
    {
        return Err(ToolError::new(format!(
            "File too large for symbol edit: {} exceeds {} bytes",
            util::relative_display(base_dir, abs_path),
            MAX_FILE_BYTES
        )));
    }
    let bytes = fs::read(abs_path)
        .map_err(|e| ToolError::new(format!("Failed to read {}: {e}", abs_path.display())))?;
    let (bom, decoded) = util::decode_bytes(&bytes);
    let crlf = decoded.contains("\r\n");
    let clean_lf = decoded.replace("\r\n", "\n");
    Ok(FileSnapshot {
        abs_path: abs_path.to_path_buf(),
        display_rel: util::relative_display(base_dir, abs_path),
        bom,
        crlf,
        clean_lf,
    })
}

fn format_with_bom_eol(lf_text: &str, bom: bool, crlf: bool) -> String {
    let eol_text = if crlf {
        lf_text.replace('\n', "\r\n")
    } else {
        lf_text.to_string()
    };
    util::join_bom(&eol_text, bom)
}

fn split_symbol_and_file(symbol: &str, path_arg: Option<&str>) -> (String, Option<String>) {
    if let Some(p) = path_arg {
        let stripped = symbol
            .rsplit_once(':')
            .filter(|(lhs, rhs)| {
                !lhs.is_empty()
                    && !rhs.is_empty()
                    && !rhs.contains(['/', '\\'])
                    && !lhs.starts_with('@')
                    && !symbol.contains("::")
            })
            .map_or(symbol, |(_, rhs)| rhs);
        return (stripped.to_string(), Some(p.to_string()));
    }
    if symbol.starts_with('@') || symbol.contains("::") {
        return (symbol.to_string(), None);
    }
    if let Some((file_part, sym_part)) = symbol.rsplit_once(':')
        && !file_part.is_empty()
        && !sym_part.is_empty()
        && !sym_part.contains(['/', '\\'])
    {
        return (sym_part.to_string(), Some(file_part.to_string()));
    }
    (symbol.to_string(), None)
}

fn build_preview_symbol_arg(symbol: &str, path_arg: Option<&str>) -> String {
    if symbol.starts_with('@') {
        return symbol.to_string();
    }
    if let Some(p) = path_arg
        && !symbol.contains(':')
    {
        return format!("{p}:{symbol}");
    }
    symbol.to_string()
}

#[allow(clippy::too_many_arguments)]
async fn try_ripwire_symbol_edit(
    ctx: &ToolContext,
    root: &Path,
    scope_path: &Path,
    op: SymbolOp,
    symbol: &str,
    path_arg: Option<&str>,
    body_text: Option<&str>,
    post_check: bool,
) -> Result<Option<ToolResult>, ToolError> {
    let Some(binary) = find_ripwire_binary() else {
        return Ok(None);
    };

    let cancel = ctx.cancel.clone().unwrap_or_default();
    let timeout = configured_ripwire_timeout();

    if op == SymbolOp::Preview {
        let sym_arg = build_preview_symbol_arg(symbol, path_arg);
        let mut args = Map::new();
        args.insert("path".to_string(), json!(root));
        args.insert("symbol".to_string(), json!(sym_arg));
        if let Some(body) = body_text {
            args.insert("new_body".to_string(), json!(body));
        }
        return match call_mcp_tool_with_scip(
            &binary,
            root,
            None,
            &cancel,
            timeout,
            op.mcp_tool(),
            &Value::Object(args),
        )
        .await
        {
            Ok(out) if !out.text.trim().is_empty() => Ok(Some(
                ToolResult::text(out.text.trim().to_string()).with_metadata(json!({
                    "operation": "preview",
                    "engine": "ripwire",
                    "transport": "mcp",
                    "symbol": symbol,
                })),
            )),
            Ok(_) => Ok(None),
            Err(err) => handle_mcp_err("preview", err),
        };
    }

    // Best-effort pre-edit snapshot + intra-process lock if we can identify the file first.
    let candidate = resolve_unique_candidate_abs(ctx, root, scope_path, symbol, path_arg);
    let _lock = match candidate.as_ref() {
        Some(p) if p.is_file() => Some(util::lock_file(p).await),
        _ => None,
    };
    let before_snap = candidate
        .as_ref()
        .filter(|p| p.is_file())
        .and_then(|p| read_snapshot(&ctx.directory, p).ok());

    let (mcp_sym, mcp_file) = split_symbol_and_file(symbol, path_arg);
    let mut args = Map::new();
    args.insert("path".to_string(), json!(root));
    args.insert("symbol".to_string(), json!(mcp_sym));
    if let Some(f) = mcp_file {
        args.insert("file".to_string(), json!(f));
    }
    let body = body_text.unwrap_or_default();
    match op {
        SymbolOp::Replace => {
            args.insert("new_body".to_string(), json!(body));
        }
        SymbolOp::InsertBefore | SymbolOp::InsertAfter => {
            args.insert("text".to_string(), json!(body));
        }
        SymbolOp::Preview => {}
    }
    // IMPORTANT: ripwire 0.6.8 rejects `"post_check": "true"`; omit for default true,
    // pass `"false"` only when explicitly skipping checks.
    if !post_check {
        args.insert("post_check".to_string(), json!("false"));
    }

    match call_mcp_tool_with_scip(
        &binary,
        root,
        None,
        &cancel,
        timeout,
        op.mcp_tool(),
        &Value::Object(args),
    )
    .await
    {
        Ok(out) => {
            parse_ripwire_edit_receipt(ctx, root, op, symbol, &out.text, before_snap.as_ref())
        }
        Err(err) => handle_mcp_err(op.as_str(), err),
    }
}

fn handle_mcp_err(op: &str, err: McpCallError) -> Result<Option<ToolResult>, ToolError> {
    match err {
        McpCallError::Cancelled => Err(ToolError::new("Tool execution cancelled")),
        McpCallError::Timeout { timeout } => Err(ToolError::new(format!(
            "symbol_edit ({op}) timed out after {} ms",
            timeout.as_millis()
        ))),
        McpCallError::CliOnly { .. } | McpCallError::Io { .. } => Ok(None),
        McpCallError::Tool { message, .. } => {
            let low = message.to_ascii_lowercase();
            if low.contains("ambiguous") || low.contains("multiple") {
                return Err(ToolError::new(message));
            }
            // Allow native fallback when ripwire doesn't support this language/form.
            Ok(None)
        }
    }
}

fn parse_ripwire_edit_receipt(
    ctx: &ToolContext,
    root: &Path,
    op: SymbolOp,
    symbol: &str,
    raw: &str,
    before_snap: Option<&FileSnapshot>,
) -> Result<Option<ToolResult>, ToolError> {
    let Some(val) = parse_mcp_json_text(raw) else {
        return Ok(None);
    };
    if let Some(err_msg) = val.get("error").and_then(Value::as_str) {
        let low = err_msg.to_ascii_lowercase();
        if low.contains("ambiguous") || low.contains("multiple") || low.contains("not found") {
            return Err(ToolError::new(format!("symbol_edit: {err_msg}")));
        }
        return Ok(None);
    }
    if val.get("applied").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }

    let file_rel = val.get("file").and_then(Value::as_str).unwrap_or("");
    let (display_path, files_meta) = collect_ripwire_edit_diff(ctx, root, file_rel, before_snap);

    let resolved_sym = val.get("symbol").and_then(Value::as_str).unwrap_or(symbol);
    let mut summary = format!(
        "Applied `{}` on symbol `{resolved_sym}` in {display_path}.",
        op.as_str()
    );
    if let Some(lines) = val.get("lines").and_then(Value::as_str) {
        let _ = write!(summary, " (lines {lines})");
    }

    let contract_warnings = append_edit_check_warnings(&val, &mut summary);
    let tests_to_run: Vec<String> = val
        .get("tests_to_run")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !tests_to_run.is_empty() {
        let _ = writeln!(summary, "\ntests_to_run: {}", tests_to_run.join(", "));
    }

    let mut metadata = json!({
        "operation": op.as_str(),
        "engine": "ripwire",
        "transport": "mcp",
        "symbol": resolved_sym,
        "path": display_path,
        "files": files_meta,
    });
    if !contract_warnings.is_empty() {
        metadata["contract_warnings"] = json!(contract_warnings);
    }
    if !tests_to_run.is_empty() {
        metadata["tests_to_run"] = json!(tests_to_run);
    }
    Ok(Some(ToolResult::text(summary).with_metadata(metadata)))
}

fn collect_ripwire_edit_diff(
    ctx: &ToolContext,
    root: &Path,
    file_rel: &str,
    before_snap: Option<&FileSnapshot>,
) -> (String, Vec<Value>) {
    let actual_abs = if file_rel.is_empty() {
        before_snap.map(|s| s.abs_path.clone())
    } else {
        let joined = root.join(file_rel);
        if joined.exists() {
            Some(joined)
        } else {
            let dir_joined = ctx.directory.join(file_rel);
            dir_joined.exists().then_some(dir_joined)
        }
    };

    let mut files_meta = Vec::new();
    let mut display_path = file_rel.to_string();
    if let Some(abs) = actual_abs.as_ref() {
        display_path = util::relative_display(&ctx.directory, abs);
        if let Ok(after_bytes) = fs::read(abs) {
            let (after_bom, after_dec) = util::decode_bytes(&after_bytes);
            let mut after_lf = after_dec.replace("\r\n", "\n");
            if let Some(before) = before_snap
                && canonical_eq(&before.abs_path, abs)
            {
                if before.crlf || before.bom != after_bom {
                    let restored = format_with_bom_eol(&after_lf, before.bom, before.crlf);
                    if util::atomic_write(abs, restored.as_bytes()).is_ok() {
                        after_lf = restored.trim_start_matches(util::BOM).replace("\r\n", "\n");
                    }
                }
                files_meta.push(diff::file_diff(
                    &before.display_rel,
                    &before.clean_lf,
                    &after_lf,
                    "updated",
                ));
            }
        }
    }
    (display_path, files_meta)
}

fn append_edit_check_warnings(val: &Value, summary: &mut String) -> Vec<String> {
    let mut contract_warnings = Vec::new();
    let Some(check) = val.get("edit_check") else {
        return contract_warnings;
    };
    let status = check.get("status").and_then(Value::as_str).unwrap_or("ok");
    if status != "ok" && status != "clean" {
        let msg = format!("edit_check status: {status}");
        contract_warnings.push(msg.clone());
        let _ = write!(summary, "\n{msg}");
    }
    if let Some(incompat) = check.get("incompatible").and_then(Value::as_array)
        && !incompat.is_empty()
    {
        let _ = writeln!(summary, "\nincompatible callers ({}):", incompat.len());
        for item in incompat.iter().take(6) {
            let text = item
                .as_str()
                .map_or_else(|| item.to_string(), ToString::to_string);
            contract_warnings.push(text.clone());
            let _ = writeln!(summary, "  - {text}");
        }
    }
    contract_warnings
}

fn canonical_eq(a: &Path, b: &Path) -> bool {
    let ca = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn resolve_unique_candidate_abs(
    ctx: &ToolContext,
    root: &Path,
    scope_path: &Path,
    symbol: &str,
    path_arg: Option<&str>,
) -> Option<PathBuf> {
    if path_arg.is_some() && scope_path.is_file() {
        return Some(scope_path.to_path_buf());
    }
    if let Some(rest) = symbol.strip_prefix('@')
        && let Some((file_part, _)) = rest.rsplit_once(':')
    {
        let p = util::resolve(ctx, file_part.trim());
        if p.is_file() {
            return Some(p);
        }
        let rp = root.join(file_part.trim());
        if rp.is_file() {
            return Some(rp);
        }
    }
    let locs = find_native_symbol_locations(ctx, scope_path, symbol).ok()?;
    if locs.len() == 1 {
        return Some(locs[0].abs_path.clone());
    }
    None
}

async fn execute_native_symbol_edit(
    ctx: &ToolContext,
    root: &Path,
    scope_path: &Path,
    op: SymbolOp,
    symbol: &str,
    path_arg: Option<&str>,
    body_text: Option<&str>,
) -> Result<ToolResult, ToolError> {
    let locs = resolve_native_target(ctx, root, scope_path, symbol, path_arg)?;
    let target = pick_single_location(ctx, &locs, symbol)?;

    if op == SymbolOp::Preview {
        let snap = read_snapshot(&ctx.directory, &target.abs_path)?;
        let span_lines = extract_line_range(&snap.clean_lf, target.start_line, target.end_line);
        let mut out = format!(
            "Preview for `{}` at {}:{}-{} [{}]:\nCurrent signature: {}\nSpan bytes/lines:\n{}",
            target.name,
            snap.display_rel,
            target.start_line,
            target.end_line,
            target.kind.as_str(),
            target.signature,
            span_lines,
        );
        if let Some(candidate) = body_text {
            let _ = write!(out, "\nProposed replacement:\n{}", candidate.trim_end());
        }
        return Ok(ToolResult::text(out).with_metadata(json!({
            "operation": "preview",
            "engine": "native",
            "symbol": target.name,
            "path": snap.display_rel,
            "start_line": target.start_line,
            "end_line": target.end_line,
        })));
    }

    let _guard = util::lock_file(&target.abs_path).await;
    let snap = read_snapshot(&ctx.directory, &target.abs_path)?;
    let content_lf = body_text.unwrap_or_default().replace("\r\n", "\n");
    let after_lf = splice_symbol_span(
        &snap.clean_lf,
        target.start_line,
        target.end_line,
        op,
        &content_lf,
    )?;

    if after_lf == snap.clean_lf {
        return Ok(ToolResult::text(format!(
            "No changes required for `{}` in {}.",
            target.name, snap.display_rel
        ))
        .with_metadata(json!({
            "operation": op.as_str(),
            "engine": "native",
            "symbol": target.name,
            "path": snap.display_rel,
            "files": [],
        })));
    }

    let encoded = format_with_bom_eol(&after_lf, snap.bom, snap.crlf);
    util::atomic_write(&target.abs_path, encoded.as_bytes())
        .map_err(|e| ToolError::new(format!("Failed to write {}: {e}", snap.display_rel)))?;

    let diff_entry = diff::file_diff(&snap.display_rel, &snap.clean_lf, &after_lf, "updated");
    let display_rel = relative_to(root, &target.abs_path);
    let summary = format!(
        "Applied `{}` on `{}` in {} (lines {}..={}).",
        op.as_str(),
        target.name,
        snap.display_rel,
        target.start_line,
        target.end_line
    );

    Ok(ToolResult::text(summary).with_metadata(json!({
        "operation": op.as_str(),
        "engine": "native",
        "symbol": target.name,
        "path": snap.display_rel,
        "repo_rel": display_rel,
        "files": [diff_entry],
    })))
}

fn resolve_native_target(
    ctx: &ToolContext,
    root: &Path,
    scope_path: &Path,
    symbol: &str,
    path_arg: Option<&str>,
) -> Result<Vec<NativeSymbolLocation>, ToolError> {
    let query = if let Some(p) = path_arg {
        let stripped = symbol
            .rsplit_once(':')
            .filter(|(lhs, rhs)| {
                !lhs.is_empty()
                    && !rhs.is_empty()
                    && !rhs.contains(['/', '\\'])
                    && !lhs.starts_with('@')
                    && !symbol.contains("::")
            })
            .map_or(symbol, |(_, rhs)| rhs);
        let scope_target = util::resolve(ctx, p);
        let locs = find_native_symbol_locations(ctx, &scope_target, stripped)?;
        if !locs.is_empty() {
            return Ok(locs);
        }
        stripped.to_string()
    } else {
        symbol.to_string()
    };

    let mut locs = find_native_symbol_locations(ctx, scope_path, &query)?;
    if locs.is_empty() && path_arg.is_none() {
        locs = find_native_symbol_locations(ctx, root, &query)?;
    }
    Ok(locs)
}

fn pick_single_location<'a>(
    ctx: &ToolContext,
    locs: &'a [NativeSymbolLocation],
    query: &str,
) -> Result<&'a NativeSymbolLocation, ToolError> {
    if locs.is_empty() {
        return Err(ToolError::new(format!(
            "Symbol `{query}` not found. Pass `path` or `@file:line` to target it explicitly."
        )));
    }
    if locs.len() == 1 {
        return Ok(&locs[0]);
    }

    // Prefer exact bare-name matches before rejecting ambiguity.
    let short_query = query.rsplit([':', '.']).next().unwrap_or(query).trim();
    let exact: Vec<&NativeSymbolLocation> = locs
        .iter()
        .filter(|l| l.name.eq_ignore_ascii_case(short_query))
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }

    let candidates = if exact.is_empty() {
        locs.iter().collect::<Vec<_>>()
    } else {
        exact
    };
    let mut msg = format!(
        "Symbol `{query}` matched {} definitions; pass `path` or `@file:line` to disambiguate:\n",
        candidates.len()
    );
    for loc in candidates.iter().take(8) {
        let rel = util::relative_display(&ctx.directory, &loc.abs_path);
        let _ = writeln!(
            msg,
            "  - {}:{} [{}] {}",
            rel,
            loc.start_line,
            loc.kind.as_str(),
            loc.signature
        );
    }
    Err(ToolError::new(msg))
}

fn splice_symbol_span(
    source_lf: &str,
    start_line: usize,
    end_line: usize,
    op: SymbolOp,
    content_lf: &str,
) -> Result<String, ToolError> {
    let lines: Vec<&str> = source_lf.split_inclusive('\n').collect();
    if start_line == 0 || start_line > lines.len() || end_line < start_line {
        return Err(ToolError::new(format!(
            "Invalid symbol line span {start_line}..={end_line} for file with {} lines",
            lines.len()
        )));
    }
    let capped_end = end_line.min(lines.len());
    let start_idx = start_line - 1;
    let end_idx_excl = capped_end;

    let prefix_bytes: usize = lines[..start_idx].iter().map(|l| l.len()).sum();
    let span_end_bytes: usize = lines[..end_idx_excl].iter().map(|l| l.len()).sum();

    let span_ended_with_nl = source_lf[..span_end_bytes].ends_with('\n');
    let mut normalized = content_lf.to_string();
    if (op == SymbolOp::InsertBefore || span_ended_with_nl) && !normalized.ends_with('\n') {
        normalized.push('\n');
    }

    let mut out = String::with_capacity(source_lf.len() + normalized.len() + 2);
    match op {
        SymbolOp::Replace => {
            out.push_str(&source_lf[..prefix_bytes]);
            out.push_str(&normalized);
            out.push_str(&source_lf[span_end_bytes..]);
        }
        SymbolOp::InsertBefore => {
            out.push_str(&source_lf[..prefix_bytes]);
            out.push_str(&normalized);
            out.push_str(&source_lf[prefix_bytes..]);
        }
        SymbolOp::InsertAfter => {
            out.push_str(&source_lf[..span_end_bytes]);
            if !source_lf[..span_end_bytes].ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&normalized);
            out.push_str(&source_lf[span_end_bytes..]);
        }
        SymbolOp::Preview => return Ok(source_lf.to_string()),
    }
    Ok(out)
}

fn extract_line_range(source_lf: &str, start_line: usize, end_line: usize) -> String {
    source_lf
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let ln = idx + 1;
            (ln >= start_line && ln <= end_line).then(|| format!("L{ln}: {line}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn native_replace_insert_before_insert_after_preserve_bom_and_crlf() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("sample.rs");
        let original = "\u{feff}fn alpha() {\r\n    let _s = \"}\";\r\n    let x = 1;\r\n}\r\n\r\nfn beta() -> i32 {\r\n    2\r\n}\r\n";
        fs::write(&src, original.as_bytes()).unwrap();

        let ctx = ToolContext {
            session_id: "s1".to_string(),
            directory: dir.path().to_path_buf(),
            cwd: dir.path().to_path_buf(),
            cancel: None,
        };

        // Test pure-Rust span engine directly so results are 100% deterministic
        // even when `ripwire` is installed on PATH.
        let prev = execute_native_symbol_edit(
            &ctx,
            dir.path(),
            &src,
            SymbolOp::Preview,
            "alpha",
            Some("sample.rs"),
            Some("fn alpha() {\n    let x = 99;\n}"),
        )
        .await
        .unwrap();
        assert!(prev.output.unwrap().contains("fn alpha()"));
        assert_eq!(fs::read_to_string(&src).unwrap(), original);

        // Replace `alpha` body (note the `}` inside the string literal must NOT end the span early!).
        let replaced = execute_native_symbol_edit(
            &ctx,
            dir.path(),
            &src,
            SymbolOp::Replace,
            "alpha",
            Some("sample.rs"),
            Some("fn alpha() {\n    let x = 99;\n}"),
        )
        .await
        .unwrap();
        let meta = replaced.metadata.unwrap();
        assert_eq!(meta["files"].as_array().unwrap().len(), 1);
        let after_replace = fs::read_to_string(&src).unwrap();
        assert!(after_replace.starts_with('\u{feff}'));
        assert!(after_replace.contains("\r\n"));
        assert!(!after_replace.contains("let x = 1;"));
        assert!(after_replace.contains("fn beta() -> i32 {\r\n    2\r\n}"));

        // Insert doc comment before `beta` and a helper after `beta`.
        execute_native_symbol_edit(
            &ctx,
            dir.path(),
            &src,
            SymbolOp::InsertBefore,
            "beta",
            Some("sample.rs"),
            Some("/// Doc above beta."),
        )
        .await
        .unwrap();
        execute_native_symbol_edit(
            &ctx,
            dir.path(),
            &src,
            SymbolOp::InsertAfter,
            "beta",
            Some("sample.rs"),
            Some("\nfn gamma() -> i32 {\n    3\n}"),
        )
        .await
        .unwrap();

        let final_disk = fs::read_to_string(&src).unwrap();
        assert!(final_disk.contains("/// Doc above beta.\r\nfn beta()"));
        assert!(final_disk.contains("fn gamma() -> i32 {\r\n    3\r\n}"));
        assert!(final_disk.starts_with('\u{feff}'));
    }

    #[tokio::test]
    async fn ambiguous_symbol_errors_without_editing() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/a.rs"),
            "pub fn helper() -> i32 { 1 }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/b.rs"),
            "pub fn helper() -> i32 { 2 }\n",
        )
        .unwrap();

        let ctx = ToolContext {
            session_id: "s1".to_string(),
            directory: dir.path().to_path_buf(),
            cwd: dir.path().to_path_buf(),
            cancel: None,
        };
        let err = execute_native_symbol_edit(
            &ctx,
            dir.path(),
            dir.path(),
            SymbolOp::Replace,
            "helper",
            None,
            Some("pub fn helper() -> i32 { 3 }"),
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("disambiguate") || err.message.contains("matched"));
    }

    #[test]
    fn split_symbol_and_file_handles_drive_letters_and_qualified_names() {
        assert_eq!(
            split_symbol_and_file("C:\\repo\\src\\lib.rs:alpha", None),
            (
                "alpha".to_string(),
                Some("C:\\repo\\src\\lib.rs".to_string())
            )
        );
        assert_eq!(
            split_symbol_and_file("src/lib.rs:alpha", None),
            ("alpha".to_string(), Some("src/lib.rs".to_string()))
        );
        assert_eq!(
            split_symbol_and_file("Type::method", None),
            ("Type::method".to_string(), None)
        );
        assert_eq!(
            split_symbol_and_file("C:\\repo\\src\\lib.rs:alpha", Some("C:\\repo\\src\\lib.rs")),
            (
                "alpha".to_string(),
                Some("C:\\repo\\src\\lib.rs".to_string())
            )
        );
    }
}
