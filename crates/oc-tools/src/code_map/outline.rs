//! Read-only `outline`, `symbol` and `references` operations over a file or directory tree.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::json;

use super::CodeMapTool;
use super::files::{collect_code_files, read_bounded_file, relative_to};
use super::native_index::native_outline;
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

        let (out, total_symbols, truncated) = native_outline(ctx, target_path, limit)?;
        Ok(ToolResult::text(out).with_metadata(json!({
            "operation": "outline",
            "symbols": total_symbols,
            "truncated": truncated
        })))
    }

    pub(super) fn execute_references_native(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let files = collect_code_files(ctx, target_path)?;
        let mut hits = Vec::new();

        'files: for path in files {
            if let Ok(content) = read_bounded_file(&path) {
                for (idx, line) in content.lines().enumerate() {
                    if line.contains(query) {
                        hits.push((path.clone(), idx + 1, line.trim().to_string()));
                        if hits.len() >= limit.saturating_mul(2) {
                            break 'files;
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

        let result =
            CodeMapTool::execute_references_native(&ctx(dir.path()), dir.path(), "needle", 10)
                .expect("references should not panic on multi-byte input");
        let output = result.output.expect("text output");
        assert!(output.contains("..."), "preview is truncated: {output}");
    }

    /// An unbounded `limit` must not overflow the internal `limit * 2` hit cap.
    #[test]
    fn references_tolerates_an_unbounded_limit() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("src.rs"), "needle\n").unwrap();

        let result = CodeMapTool::execute_references_native(
            &ctx(dir.path()),
            dir.path(),
            "needle",
            usize::MAX,
        )
        .expect("an unbounded limit should not overflow");
        let metadata = result.metadata.expect("metadata");
        assert_eq!(metadata["matches"], 1);
    }

    #[test]
    fn outline_reports_truncation_when_first_file_hits_exact_limit() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn one() {}\npub fn two() {}\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.rs"), "pub fn three() {}\n").unwrap();

        let result = CodeMapTool::execute_outline(&ctx(dir.path()), dir.path(), 2)
            .expect("outline should succeed");
        let output = result.output.expect("text output");
        let metadata = result.metadata.expect("metadata");
        assert_eq!(metadata["symbols"], 3);
        assert_eq!(metadata["truncated"], true);
        assert!(output.contains("showing 2 of 3 symbols"), "{output}");
    }

    #[test]
    fn references_stops_scanning_across_files_at_twice_limit() {
        let dir = TempDir::new().unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            fs::write(dir.path().join(name), "needle 1\nneedle 2\n").unwrap();
        }

        let result =
            CodeMapTool::execute_references_native(&ctx(dir.path()), dir.path(), "needle", 1)
                .expect("references should succeed");
        let metadata = result.metadata.expect("metadata");
        assert_eq!(metadata["matches"], 2);
        assert_eq!(metadata["truncated"], true);
    }
}
