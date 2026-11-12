//! Read-only `outline`, `symbol` and `references` operations over a file or directory tree.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::json;

use super::CodeMapTool;
use super::files::{collect_code_files, read_bounded_file, relative_to};
use super::parsers::parse_symbols;
use crate::{ToolContext, ToolError, ToolResult};

impl CodeMapTool {
    pub(super) fn execute_outline(
        ctx: &ToolContext,
        target_path: &Path,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        if target_path.is_file() {
            let content = read_bounded_file(target_path)?;
            let symbols = parse_symbols(
                &content,
                target_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or(""),
            );
            let rel = relative_to(&ctx.directory, target_path);
            let mut out = format!("Outline for {rel} ({} symbols):\n", symbols.len());
            for sym in symbols.iter().take(limit) {
                let _ = writeln!(
                    out,
                    "  L{:<4} [{}] {}",
                    sym.line,
                    sym.kind.as_str(),
                    sym.signature
                );
            }
            if symbols.len() > limit {
                let _ = writeln!(
                    out,
                    "  [showing lines 1-{} of {}; truncated]",
                    limit,
                    symbols.len()
                );
            }
            let truncated = symbols.len() > limit;
            return Ok(ToolResult::text(out).with_metadata(json!({
                "operation": "outline",
                "symbols": symbols.len(),
                "truncated": truncated
            })));
        }

        // Directory outline
        let files = collect_code_files(ctx, target_path)?;
        let mut out = format!(
            "Directory outline for {} ({} files):\n",
            target_path.display(),
            files.len()
        );
        let mut total_symbols = 0;
        let mut shown_symbols = 0;

        for path in files {
            if let Ok(content) = read_bounded_file(&path) {
                let symbols = parse_symbols(
                    &content,
                    path.extension().and_then(|ext| ext.to_str()).unwrap_or(""),
                );
                if symbols.is_empty() {
                    continue;
                }
                total_symbols += symbols.len();
                let rel = relative_to(&ctx.directory, &path);
                let _ = writeln!(out, "\n{rel} ({} symbols):", symbols.len());
                for sym in &symbols {
                    if shown_symbols >= limit {
                        break;
                    }
                    let _ = writeln!(
                        out,
                        "  L{:<4} [{}] {}",
                        sym.line,
                        sym.kind.as_str(),
                        sym.signature
                    );
                    shown_symbols += 1;
                }
                if shown_symbols >= limit {
                    break;
                }
            }
        }

        if shown_symbols < total_symbols {
            let _ = writeln!(
                out,
                "\n[showing {shown_symbols} of {total_symbols} symbols; pass a specific file `path` to narrow down]"
            );
        }

        Ok(ToolResult::text(out).with_metadata(json!({
            "operation": "outline",
            "symbols": total_symbols,
            "truncated": shown_symbols < total_symbols
        })))
    }

    pub(super) fn execute_symbol(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let q_lower = query.to_lowercase();
        let files = collect_code_files(ctx, target_path)?;
        let mut matches = Vec::new();

        for path in files {
            if let Ok(content) = read_bounded_file(&path) {
                let symbols = parse_symbols(
                    &content,
                    path.extension().and_then(|ext| ext.to_str()).unwrap_or(""),
                );
                for sym in symbols {
                    let s_name_lower = sym.name.to_lowercase();
                    if s_name_lower == q_lower || s_name_lower.contains(&q_lower) {
                        let is_exact = s_name_lower == q_lower;
                        matches.push((is_exact, path.clone(), sym));
                    }
                }
            }
        }

        // Sort exact matches first, then by line count
        matches.sort_by_key(|a| std::cmp::Reverse(a.0));

        let total = matches.len();
        let mut out = format!("Symbol matches for \"{query}\" ({total} found):\n");
        if total == 0 {
            out.push_str("  No matching symbol definitions found.\n");
        } else {
            for (_, path, sym) in matches.iter().take(limit) {
                let rel = relative_to(&ctx.directory, path);
                let _ = writeln!(
                    out,
                    "  {}:{:<4} [{}] {}",
                    rel,
                    sym.line,
                    sym.kind.as_str(),
                    sym.signature
                );
            }
            if total > limit {
                let _ = writeln!(out, "  [showing {limit} of {total} matches; truncated]");
            }
        }

        Ok(ToolResult::text(out).with_metadata(json!({
            "operation": "symbol",
            "matches": total,
            "truncated": total > limit
        })))
    }

    pub(super) fn execute_references(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let files = collect_code_files(ctx, target_path)?;
        let mut hits = Vec::new();

        for path in files {
            if let Ok(content) = read_bounded_file(&path) {
                for (idx, line) in content.lines().enumerate() {
                    if line.contains(query) {
                        hits.push((path.clone(), idx + 1, line.trim().to_string()));
                        if hits.len() >= limit.saturating_mul(2) {
                            break;
                        }
                    }
                }
            }
        }

        let total = hits.len();
        let mut out = format!("References to \"{query}\" ({total} hits):\n");
        if total == 0 {
            out.push_str("  No references found.\n");
        } else {
            for (path, line_no, text) in hits.iter().take(limit) {
                let rel = relative_to(&ctx.directory, path);
                let preview = if text.len() > 100 {
                    // Back off to a char boundary: byte 100 can land inside a
                    // multi-byte character and panic the slice.
                    let mut end = 100;
                    while end > 0 && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &text[..end])
                } else {
                    text.clone()
                };
                let _ = writeln!(out, "  {rel}:{line_no:<4} {preview}");
            }
            if total > limit {
                let _ = writeln!(out, "  [showing {limit} of {total} references; truncated]");
            }
        }

        Ok(ToolResult::text(out).with_metadata(json!({
            "operation": "references",
            "matches": total,
            "truncated": total > limit
        })))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_string(),
            directory: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            cancel: None,
        }
    }

    /// A reference line longer than 100 bytes whose byte 100 falls inside a
    /// multi-byte character must be previewed without panicking.
    #[test]
    fn references_preview_truncates_on_a_char_boundary() {
        let dir = TempDir::new().unwrap();
        // 99 ASCII bytes, then a 3-byte ellipsis at 99..102, so byte 100 is
        // inside it. The query is present so the line becomes a hit.
        let line = format!("needle{}…tail", "a".repeat(93));
        assert!(line.len() > 100);
        assert!(line.is_char_boundary(99));
        assert!(!line.is_char_boundary(100));
        fs::write(dir.path().join("src.rs"), &line).unwrap();

        let result = CodeMapTool::execute_references(&ctx(dir.path()), dir.path(), "needle", 10)
            .expect("references should not panic on multi-byte input");
        let output = result.output.expect("text output");
        assert!(output.contains("..."), "preview is truncated: {output}");
    }

    /// An unbounded `limit` must not overflow the internal `limit * 2` hit cap.
    #[test]
    fn references_tolerates_an_unbounded_limit() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("src.rs"), "needle\n").unwrap();

        let result =
            CodeMapTool::execute_references(&ctx(dir.path()), dir.path(), "needle", usize::MAX)
                .expect("an unbounded limit should not overflow");
        let metadata = result.metadata.expect("metadata");
        assert_eq!(metadata["matches"], 1);
    }
}
