//! `code_map` tool: structural code intelligence, file outlines, symbol definitions,
//! and task-relevant code maps.
//!
//! Provides pure-Rust AST-like symbol extraction for Rust, TypeScript, JavaScript,
//! Python, Go, C/C++, and Markdown without external dependencies, and opportunistically
//! delegates to `ripwire` when present on the system.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Serialize;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

pub const DEFAULT_LIMIT: usize = 50;
pub const DEFAULT_MAX_TOKENS: usize = 1200;
const MAX_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024; // 4 MB cap

/// One-line installation command for `redhat-et/ripwire` (zero-dep C++23 graph context engine).
pub const RIPWIRE_INSTALL_CMD: &str =
    "curl -fsSL https://raw.githubusercontent.com/redhat-et/ripwire/main/install.sh | sh";

#[derive(Debug, Default, Clone, Copy)]
pub struct CodeMapTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    Type,
    Module,
    Heading,
}

impl SymbolKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Interface => "interface",
            Self::Class => "class",
            Self::Type => "type",
            Self::Module => "module",
            Self::Heading => "heading",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub signature: String,
    pub line: usize,
    pub doc: Option<String>,
    pub visibility: Visibility,
}

#[async_trait]
impl Tool for CodeMapTool {
    fn name(&self) -> &str {
        "code_map"
    }

    fn description(&self) -> &str {
        "Extract code structure, file outlines, symbol definitions, and task-relevant \
         code maps without reading entire files. Supports Rust, TypeScript, JavaScript, \
         Python, Go, C/C++, and Markdown natively. If ripwire is installed (via \
         `curl -fsSL https://raw.githubusercontent.com/redhat-et/ripwire/main/install.sh | sh`), \
         accelerates with deep Personalized PageRank call graphs."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["outline", "map", "symbol", "references"],
                    "description": "Operation: 'outline' (file/dir signatures & symbols), \
                                    'map' (ranked structural map for a query/task), \
                                    'symbol' (find symbol definitions), or \
                                    'references' (find symbol usages across files)."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to inspect. Defaults to the working directory."
                },
                "query": {
                    "type": "string",
                    "description": "Symbol name, keyword, or task description to search or rank by."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of results to return (default: 50)."
                },
                "max_tokens": {
                    "type": "integer",
                    "minimum": 100,
                    "description": "Approximate token budget for the generated code map (default: 1200)."
                }
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let operation = util::required_string(&input, "operation")?;
        let rel_path = util::optional_string(&input, "path")?;
        let query = util::optional_string(&input, "query")?;
        let limit = util::optional_u64(&input, "limit")?.map_or(DEFAULT_LIMIT, |val| {
            usize::try_from(val).unwrap_or(usize::MAX)
        });
        let max_tokens = util::optional_u64(&input, "max_tokens")?
            .map_or(DEFAULT_MAX_TOKENS, |val| {
                usize::try_from(val).unwrap_or(usize::MAX)
            });

        let target_path = if let Some(path_str) = rel_path {
            let p = Path::new(&path_str);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                ctx.directory.join(p)
            }
        } else {
            ctx.directory.clone()
        };

        if !target_path.exists() {
            return Err(ToolError::new(format!(
                "Path does not exist: {}",
                target_path.display()
            )));
        }

        match operation.as_str() {
            "outline" => Self::execute_outline(ctx, &target_path, limit),
            "symbol" => {
                let q = query.ok_or_else(|| {
                    ToolError::new("Parameter `query` is required for operation 'symbol'")
                })?;
                Self::execute_symbol(ctx, &target_path, &q, limit)
            }
            "references" => {
                let q = query.ok_or_else(|| {
                    ToolError::new("Parameter `query` is required for operation 'references'")
                })?;
                Self::execute_references(ctx, &target_path, &q, limit)
            }
            "map" => {
                let q = query.as_deref().unwrap_or_default();
                Self::execute_map(ctx, &target_path, q, max_tokens, limit)
            }
            other => Err(ToolError::new(format!(
                "Unsupported operation '{other}'. Expected 'outline', 'map', 'symbol', or 'references'."
            ))),
        }
    }
}

type ScoredFile = (f32, PathBuf, Vec<(f32, Symbol)>);

impl CodeMapTool {
    fn execute_outline(
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

    fn execute_symbol(
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

    fn execute_references(
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
                        if hits.len() >= limit * 2 {
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
                    format!("{}...", &text[..100])
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

    #[allow(clippy::too_many_lines)]
    fn execute_map(
        ctx: &ToolContext,
        target_path: &Path,
        query: &str,
        max_tokens: usize,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        // Opportunistic ripwire acceleration if available
        if let Some(ripwire_path) = find_ripwire_binary()
            && !query.is_empty()
        {
            let mut cmd = Command::new(ripwire_path);
            cmd.arg(target_path);
            cmd.arg(format!("--for={query}"));
            cmd.arg(format!("--max-tokens={max_tokens}"));
            if let Ok(output) = cmd.output()
                && output.status.success()
            {
                let text = String::from_utf8_lossy(&output.stdout).to_string();
                if !text.trim().is_empty() {
                    return Ok(ToolResult::text(text).with_metadata(json!({
                        "operation": "map",
                        "engine": "ripwire",
                        "query": query
                    })));
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

        let max_chars = max_tokens * 4;
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
        let _ = writeln!(
            out,
            "\n[Tip: For deep call graphs and Personalized PageRank, install ripwire: `{RIPWIRE_INSTALL_CMD}`]"
        );

        Ok(ToolResult::text(out).with_metadata(json!({
            "operation": "map",
            "engine": "native",
            "query": query,
            "files": total_files_emitted,
            "symbols": total_symbols_emitted,
            "ripwire_install_command": RIPWIRE_INSTALL_CMD
        })))
    }
}

/// Helper to read a file up to `MAX_FILE_SIZE_BYTES`.
fn read_bounded_file(path: &Path) -> Result<String, ToolError> {
    let metadata = fs::metadata(path).map_err(|e| ToolError::new(e.to_string()))?;
    if metadata.len() > MAX_FILE_SIZE_BYTES {
        return Err(ToolError::new(format!(
            "File exceeds maximum analysis size (4 MB): {}",
            path.display()
        )));
    }
    fs::read_to_string(path).map_err(|e| ToolError::new(e.to_string()))
}

/// Collect source files respecting .gitignore, ignoring hidden directories.
fn collect_code_files(ctx: &ToolContext, root: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .parents(true)
        .build();

    for entry in walker {
        if let Some(ref cancel) = ctx.cancel
            && cancel.is_cancelled()
        {
            return Err(ToolError::new("Tool execution cancelled"));
        }
        let Ok(entry) = entry else { continue };
        let p = entry.path();
        if p.is_file() && is_code_extension(p) {
            files.push(p.to_path_buf());
        }
    }
    Ok(files)
}

fn is_code_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "c"
            | "cpp"
            | "cc"
            | "h"
            | "hpp"
            | "md"
            | "toml"
            | "json"
    )
}

fn relative_to(root: &Path, target: &Path) -> String {
    if let Ok(rel) = target.strip_prefix(root) {
        rel.to_string_lossy().into_owned()
    } else {
        target.to_string_lossy().into_owned()
    }
}

fn find_ripwire_binary() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("RIPWIRE_PATH") {
        let p = PathBuf::from(custom);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = PathBuf::from(home).join(".local/bin/ripwire");
        if p.exists() {
            return Some(p);
        }
    }
    let local = PathBuf::from("/root/.local/bin/ripwire");
    if local.exists() {
        return Some(local);
    }
    // Check PATH
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("ripwire");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Pure-Rust multi-language parser for signatures and symbols.
#[must_use]
pub fn parse_symbols(content: &str, ext: &str) -> Vec<Symbol> {
    match ext.to_lowercase().as_str() {
        "rs" => parse_rust_symbols(content),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => parse_ts_js_symbols(content),
        "py" => parse_python_symbols(content),
        "go" => parse_go_symbols(content),
        "c" | "cpp" | "cc" | "h" | "hpp" => parse_c_cpp_symbols(content),
        "md" => parse_markdown_headings(content),
        _ => Vec::new(),
    }
}

fn strip_rust_qualifiers(mut s: &str) -> (&str, Visibility) {
    let mut vis = Visibility::Private;
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("pub(crate)") {
            s = rest;
            vis = Visibility::Public;
        } else if let Some(rest) = s.strip_prefix("pub(super)") {
            s = rest;
            vis = Visibility::Public;
        } else if let Some(rest) = s.strip_prefix("pub ") {
            s = rest;
            vis = Visibility::Public;
        } else if let Some(rest) = s.strip_prefix("async ") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("const ") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("unsafe ") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("extern ") {
            s = rest;
        } else {
            break;
        }
    }
    (s.trim_start(), vis)
}

fn parse_rust_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut doc_acc: Option<String> = None;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("///") || trimmed.starts_with("//!") {
            let doc_text = trimmed
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim();
            doc_acc = Some(
                doc_acc.map_or_else(|| doc_text.to_string(), |acc| format!("{acc} {doc_text}")),
            );
            continue;
        }

        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        let (rest, vis) = strip_rust_qualifiers(trimmed);

        if let Some(after) = rest.strip_prefix("fn ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("struct ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Struct,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("enum ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Enum,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("trait ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Trait,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("type ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Type,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("mod ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Module,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("impl ") {
            let name_str = clean_signature(after);
            symbols.push(Symbol {
                name: name_str,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: doc_acc.take(),
                visibility: vis,
            });
        } else {
            doc_acc = None;
        }
    }

    symbols
}

fn parse_ts_js_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        let is_export = trimmed.starts_with("export ");
        let vis = if is_export {
            Visibility::Public
        } else {
            Visibility::Private
        };
        let rest = trimmed
            .trim_start_matches("export")
            .trim_start_matches("default")
            .trim();

        if let Some(after) = rest.strip_prefix("function ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("async function ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("class ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("interface ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Interface,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = rest.strip_prefix("type ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Type,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if is_export && (rest.starts_with("const ") || rest.starts_with("let ")) {
            let after = rest
                .trim_start_matches("const ")
                .trim_start_matches("let ")
                .trim();
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Function,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        }
    }

    symbols
}

fn parse_python_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("def ") || trimmed.starts_with("async def ") {
            let after = trimmed
                .trim_start_matches("async ")
                .trim_start_matches("def ")
                .trim();
            let name = extract_ident(after);
            let is_method = line.starts_with("    ") || line.starts_with('\t');
            let vis = if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            };
            symbols.push(Symbol {
                name,
                kind: if is_method {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = trimmed.strip_prefix("class ") {
            let name = extract_ident(after);
            let vis = if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            };
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        }
    }

    symbols
}

fn parse_go_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("func ") {
            let (kind, name) = if after.starts_with('(') {
                // Method: func (r *Receiver) Name(...)
                let method_name = after
                    .split(')')
                    .nth(1)
                    .map_or_else(|| "method".to_string(), |s| extract_ident(s.trim()));
                (SymbolKind::Method, method_name)
            } else {
                (SymbolKind::Function, extract_ident(after))
            };
            let vis = if name.chars().next().is_some_and(char::is_uppercase) {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(Symbol {
                name,
                kind,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        } else if let Some(after) = trimmed.strip_prefix("type ") {
            let name = extract_ident(after);
            let kind = if trimmed.contains("interface") {
                SymbolKind::Interface
            } else {
                SymbolKind::Struct
            };
            let vis = if name.chars().next().is_some_and(char::is_uppercase) {
                Visibility::Public
            } else {
                Visibility::Private
            };
            symbols.push(Symbol {
                name,
                kind,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: vis,
            });
        }
    }

    symbols
}

fn parse_c_cpp_symbols(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("class ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Class,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: Visibility::Public,
            });
        } else if let Some(after) = trimmed.strip_prefix("struct ") {
            let name = extract_ident(after);
            symbols.push(Symbol {
                name,
                kind: SymbolKind::Struct,
                signature: clean_signature(trimmed),
                line: idx + 1,
                doc: None,
                visibility: Visibility::Public,
            });
        }
    }

    symbols
}

fn parse_markdown_headings(content: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            let text = trimmed.trim_start_matches('#').trim();
            symbols.push(Symbol {
                name: text.to_string(),
                kind: SymbolKind::Heading,
                signature: format!("{} {}", "#".repeat(level), text),
                line: idx + 1,
                doc: None,
                visibility: Visibility::Public,
            });
        }
    }

    symbols
}

fn extract_ident(input: &str) -> String {
    input
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

fn clean_signature(line: &str) -> String {
    let trimmed = line.trim();
    let without_brace = if let Some(stripped) = trimmed.strip_suffix('{') {
        stripped.trim()
    } else {
        trimmed
    };
    if without_brace.len() > 140 {
        format!("{}...", &without_brace[..137])
    } else {
        without_brace.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_symbols_with_visibility_and_docs() {
        let src = r#"
/// Service context for managing sessions.
pub struct SessionService {
    id: String,
}

pub trait SessionManager {
    fn run(&self);
}

impl SessionManager for SessionService {
    pub async fn run(&self) {
        println!("running");
    }
}

fn private_helper() -> bool {
    true
}
"#;
        let symbols = parse_rust_symbols(src);
        assert_eq!(symbols.len(), 6);

        assert_eq!(symbols[0].name, "SessionService");
        assert_eq!(symbols[0].kind, SymbolKind::Struct);
        assert_eq!(symbols[0].visibility, Visibility::Public);
        assert!(symbols[0].doc.as_ref().unwrap().contains("Service context"));

        assert_eq!(symbols[1].name, "SessionManager");
        assert_eq!(symbols[1].kind, SymbolKind::Trait);

        assert_eq!(symbols[2].name, "run");
        assert_eq!(symbols[2].kind, SymbolKind::Function);

        assert_eq!(symbols[3].name, "SessionManager for SessionService");
        assert_eq!(symbols[3].kind, SymbolKind::Class);

        assert_eq!(symbols[4].name, "run");
        assert_eq!(symbols[4].kind, SymbolKind::Function);
        assert_eq!(symbols[4].visibility, Visibility::Public);

        assert_eq!(symbols[5].name, "private_helper");
        assert_eq!(symbols[5].kind, SymbolKind::Function);
        assert_eq!(symbols[5].visibility, Visibility::Private);
    }

    #[test]
    fn parses_ts_and_python_symbols() {
        let ts_src = r#"
export interface UserSession { id: string; }
export class EngineRunner extends Runner {
    constructor() {}
}
export async function startEngine(port: number): Promise<void> {}
const secretKey = "xyz";
"#;
        let ts_symbols = parse_ts_js_symbols(ts_src);
        assert_eq!(ts_symbols.len(), 3);
        assert_eq!(ts_symbols[0].name, "UserSession");
        assert_eq!(ts_symbols[0].kind, SymbolKind::Interface);
        assert_eq!(ts_symbols[1].name, "EngineRunner");
        assert_eq!(ts_symbols[1].kind, SymbolKind::Class);
        assert_eq!(ts_symbols[2].name, "startEngine");
        assert_eq!(ts_symbols[2].kind, SymbolKind::Function);

        let py_src = r"
class Controller:
    def handle_request(self, req):
        pass

async def serve_forever():
    pass
";
        let py_symbols = parse_python_symbols(py_src);
        assert_eq!(py_symbols.len(), 3);
        assert_eq!(py_symbols[0].name, "Controller");
        assert_eq!(py_symbols[0].kind, SymbolKind::Class);
        assert_eq!(py_symbols[1].name, "handle_request");
        assert_eq!(py_symbols[1].kind, SymbolKind::Method);
        assert_eq!(py_symbols[2].name, "serve_forever");
        assert_eq!(py_symbols[2].kind, SymbolKind::Function);
    }
}
