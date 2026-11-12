//! `map` operation: ripwire delegation when available, then the native scored structural map.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;

use super::files::{collect_code_files, read_bounded_file, relative_to};
use super::parsers::parse_symbols;
use super::ripwire::{
    RIPWIRE_INSTALL_CMD, RipwireFailure, find_ripwire_binary, run_ripwire, trimmed_stderr,
};
use super::{CodeMapTool, Symbol, SymbolKind, Visibility};
use crate::{ToolContext, ToolError, ToolResult};

type ScoredFile = (f32, PathBuf, Vec<(f32, Symbol)>);

impl CodeMapTool {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn execute_map(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        max_tokens: usize,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        // Delegation is unconditional when the binary is available: a `map`
        // without a query is ripwire's default ranked map, not a reason to keep
        // the weaker native engine.
        let ripwire_binary = find_ripwire_binary();
        let mut ripwire_failure: Option<String> = None;
        let mut ripwire_stderr: Option<String> = None;

        if let Some(ripwire_path) = ripwire_binary.as_ref() {
            match run_ripwire(ripwire_path, target_path, query, max_tokens, limit).await {
                Ok(run) => {
                    if let Some(stderr) = trimmed_stderr(&run.stderr) {
                        ripwire_stderr = Some(stderr);
                    }
                    if !run.stdout.trim().is_empty() {
                        let mut metadata = json!({
                            "operation": "map",
                            "engine": "ripwire",
                            "query": query,
                            "token_budget": max_tokens
                        });
                        // The task lens has no row window, so `limit` is only a
                        // real bound on the default map.
                        if query.is_empty() {
                            metadata["limit"] = json!(limit);
                        }
                        if let Some(stderr) = ripwire_stderr {
                            metadata["ripwire_stderr"] = json!(stderr);
                        }
                        return Ok(ToolResult::text(run.stdout).with_metadata(metadata));
                    }
                    // An empty map from a successful run is not a usable answer.
                    ripwire_failure = Some("ripwire returned no output".to_string());
                }
                Err(RipwireFailure::Timeout { timeout }) => {
                    return Err(ToolError::new(format!(
                        "ripwire did not finish within {} ms; raise OPENCODE_RIPWIRE_TIMEOUT_MS \
                         or pass `query` to narrow the scan",
                        timeout.as_millis()
                    ))
                    .with_metadata(json!({
                        "operation": "map",
                        "engine": "ripwire",
                        "timeout_ms": timeout.as_millis()
                    })));
                }
                Err(RipwireFailure::Spawn { message }) => {
                    ripwire_failure = Some(message);
                }
                Err(RipwireFailure::Exit { code, stderr }) => {
                    ripwire_stderr = trimmed_stderr(&stderr);
                    ripwire_failure = Some(format!(
                        "ripwire exited with status {}",
                        code.map_or_else(|| "signal".to_string(), |code| code.to_string())
                    ));
                }
            }
        }

        // Native pure-Rust structural code map engine
        let q_terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.trim().to_lowercase())
            .filter(|term| !term.is_empty())
            .collect();

        let files = collect_code_files(ctx, target_path)?;
        let mut file_scores: Vec<ScoredFile> = Vec::new();

        for path in files {
            if let Ok(content) = read_bounded_file(&path) {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let symbols = parse_symbols(&content, ext);
                let rel = relative_to(&ctx.directory, &path).to_lowercase();

                let mut path_score = 0.0_f32;
                for term in &q_terms {
                    if rel.contains(term) {
                        path_score += 4.0;
                    }
                }

                let mut scored_symbols = Vec::new();
                for sym in symbols {
                    let mut score = 0.0_f32;
                    let name_lower = sym.name.to_lowercase();
                    let sig_lower = sym.signature.to_lowercase();
                    let doc_lower = sym.doc.as_deref().unwrap_or("").to_lowercase();

                    if q_terms.is_empty() {
                        // Unconditional default map: boost public structs/classes/interfaces/functions
                        score = match sym.kind {
                            SymbolKind::Struct
                            | SymbolKind::Class
                            | SymbolKind::Interface
                            | SymbolKind::Trait => 3.0,
                            SymbolKind::Function | SymbolKind::Method => 2.0,
                            _ => 1.0,
                        };
                    } else {
                        for term in &q_terms {
                            if name_lower == *term {
                                score += 15.0;
                            } else if name_lower.contains(term) {
                                score += 8.0;
                            }
                            if sig_lower.contains(term) {
                                score += 3.0;
                            }
                            if doc_lower.contains(term) {
                                score += 2.0;
                            }
                        }
                    }

                    if sym.visibility == Visibility::Public {
                        score *= 1.2;
                    }

                    if score > 0.0 {
                        scored_symbols.push((score, sym));
                    }
                }

                scored_symbols.sort_by(|a, b| b.0.total_cmp(&a.0));

                let file_sym_sum: f32 = scored_symbols.iter().take(5).map(|(s, _)| *s).sum();
                let total_file_score = path_score + file_sym_sum;

                if total_file_score > 0.0 {
                    file_scores.push((total_file_score, path, scored_symbols));
                }
            }
        }

        file_scores.sort_by(|a, b| b.0.total_cmp(&a.0));

        let mut out = format!(
            "<code_map query=\"{query}\" root=\"{}\">\n",
            relative_to(&ctx.directory, target_path)
        );

        let max_chars = max_tokens.saturating_mul(4);
        let mut total_symbols_emitted = 0;
        let mut total_files_emitted = 0;

        for (_, path, syms) in file_scores.iter().take(limit) {
            if out.len() >= max_chars {
                break;
            }
            let rel = relative_to(&ctx.directory, path);
            let _ = writeln!(out, "{rel}:");
            total_files_emitted += 1;

            for (_, sym) in syms.iter().take(8) {
                if out.len() >= max_chars {
                    break;
                }
                let _ = writeln!(out, "  L{:<4} {}", sym.line, sym.signature);
                total_symbols_emitted += 1;
            }
        }

        out.push_str("</code_map>\n");

        let mut metadata = json!({
            "operation": "map",
            "engine": "native",
            "query": query,
            "files": total_files_emitted,
            "symbols": total_symbols_emitted
        });

        // The install tip is only truthful when discovery found no binary.
        if ripwire_binary.is_none() {
            let _ = writeln!(
                out,
                "\n[Tip: For deep call graphs and Personalized PageRank, install ripwire: `{RIPWIRE_INSTALL_CMD}`]"
            );
            metadata["ripwire_install_command"] = json!(RIPWIRE_INSTALL_CMD);
        } else if let Some(reason) = ripwire_failure {
            metadata["ripwire_error"] = json!(reason);
        }
        if let Some(stderr) = ripwire_stderr {
            metadata["ripwire_stderr"] = json!(stderr);
        }

        Ok(ToolResult::text(out).with_metadata(metadata))
    }
}
