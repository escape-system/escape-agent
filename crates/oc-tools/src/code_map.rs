//! `code_map` tool: structural code intelligence, file outlines, symbol definitions,
//! and task-relevant code maps.
//!
//! Provides pure-Rust AST-like symbol extraction for Rust, TypeScript, JavaScript,
//! Python, Go, C/C++, and Markdown without external dependencies, and opportunistically
//! delegates to `ripwire` when present on the system.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

pub const DEFAULT_LIMIT: usize = 50;
pub const DEFAULT_MAX_TOKENS: usize = 1200;
const MAX_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024; // 4 MB cap

/// One-line installation command for `redhat-et/ripwire` (zero-dep C++23 graph context engine).
pub const RIPWIRE_INSTALL_CMD: &str =
    "curl -fsSL https://raw.githubusercontent.com/redhat-et/ripwire/main/install.sh | sh";

/// Environment override for the hard ripwire timeout, in milliseconds.
pub const RIPWIRE_TIMEOUT_ENV: &str = "OPENCODE_RIPWIRE_TIMEOUT_MS";
/// Default hard timeout for a single ripwire invocation.
pub const DEFAULT_RIPWIRE_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on the stderr copy surfaced in result metadata.
const RIPWIRE_STDERR_LIMIT_BYTES: usize = 2 * 1024;

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
                Self::execute_map(ctx, &target_path, q, max_tokens, limit).await
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
    async fn execute_map(
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

/// Arguments passed to ripwire, plus the parsed form of the caller's request.
///
/// The flag set is **mode dependent** because ripwire reads different budgets
/// per verb. Measured on ripwire 0.6.8 over `crates/oc-tools/src`:
///
/// - `--for=TASK --max-tokens=300` exits 0 but emits the *full* bundle
///   (`--max-tokens is not read by --for`, 6825 B for 300/1200/absent alike).
/// - `--for=TASK --top-k=5` also exits 0 with the same 6825 B and a warning
///   (`--top-k is not read by --for`).
/// - `--for=TASK --token-budget=300` is honoured: 2047 B against 6825 B
///   unbudgeted, with no warning, so `--token-budget` is the only budget flag
///   the task lens reads.
/// - Without `--for`, `--max-tokens=300` gives 1357 B and `--max-tokens=3000`
///   gives 5274 B, while a bare `--token-budget=300` exits 3 (hard gate
///   exceeded), so the default map reads `--max-tokens`, not `--token-budget`.
/// - Without `--for`, `--top-k=5` gives 2022 B of ranked rows and
///   `--top-k=100` gives 8879 B, so the default map does read `--top-k`.
/// - With `--for`, `--limit=N` is accepted but switches the document schema
///   from the `<ctx>` bundle to `ripwire.candidate-page/v1`, and it is refused
///   together with `--detail`. The task lens therefore gets no row window: the
///   budget is the only bound, and `limit` caps the default/native maps only.
///
/// Passing the unread flag is not harmless: it changes nothing but writes a
/// warning to stderr that a status-only check never sees.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RipwireInvocation {
    args: Vec<String>,
}

impl RipwireInvocation {
    fn new(target: &Path, query: &str, max_tokens: usize, limit: usize) -> Self {
        let mut args = vec![target.to_string_lossy().into_owned()];
        if query.is_empty() {
            // Default ranked map: this verb has no task lens, so the token
            // budget and the row window are both honoured.
            args.push(format!("--max-tokens={max_tokens}"));
            args.push(format!("--top-k={limit}"));
        } else {
            // Task lens: --token-budget is the only budget flag it reads.
            // --max-tokens and --top-k are accepted but ignored with a warning.
            args.push(format!("--for={query}"));
            args.push(format!("--token-budget={max_tokens}"));
        }
        Self { args }
    }
}

#[derive(Debug)]
struct RipwireOutput {
    stdout: String,
    stderr: Vec<u8>,
}

#[derive(Debug)]
enum RipwireFailure {
    /// The child was still running when the deadline elapsed; it was killed.
    Timeout { timeout: Duration },
    /// The child could not be started at all.
    Spawn { message: String },
    /// The child ran and exited non-zero.
    Exit { code: Option<i32>, stderr: Vec<u8> },
}

/// Hard timeout for one ripwire run, overridable with
/// [`RIPWIRE_TIMEOUT_ENV`]. A non-numeric or zero value falls back to the
/// default rather than disabling the guard.
fn ripwire_timeout_ms(raw: Option<&OsStr>) -> u64 {
    raw.and_then(OsStr::to_str)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_RIPWIRE_TIMEOUT_MS)
}

fn configured_ripwire_timeout() -> Duration {
    let raw = std::env::var_os(RIPWIRE_TIMEOUT_ENV);
    Duration::from_millis(ripwire_timeout_ms(raw.as_deref()))
}

/// Trim captured stderr to at most [`RIPWIRE_STDERR_LIMIT_BYTES`] and return
/// `None` when it carries no text.
fn trimmed_stderr(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= RIPWIRE_STDERR_LIMIT_BYTES {
        return Some(text.to_string());
    }
    // Trim on a char boundary so lossy multi-byte input cannot panic a slice.
    let mut end = RIPWIRE_STDERR_LIMIT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &text[..end]))
}

/// Run ripwire with a hard deadline.
///
/// Both pipes are drained on dedicated blocking tasks so a child that fills a
/// pipe buffer cannot deadlock the wait loop, and `kill_on_drop` guarantees the
/// process dies when the future is dropped (caller cancelled, timeout fired).
async fn run_ripwire(
    binary: &Path,
    target: &Path,
    query: &str,
    max_tokens: usize,
    limit: usize,
) -> Result<RipwireOutput, RipwireFailure> {
    let invocation = RipwireInvocation::new(target, query, max_tokens, limit);
    let timeout = configured_ripwire_timeout();

    let mut child = Command::new(binary)
        .args(&invocation.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| RipwireFailure::Spawn {
            message: format!("failed to start {}: {error}", binary.display()),
        })?;

    let stdout = child.stdout.take().ok_or_else(|| RipwireFailure::Spawn {
        message: "ripwire stdout pipe unavailable".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| RipwireFailure::Spawn {
        message: "ripwire stderr pipe unavailable".to_string(),
    })?;
    let stdout_task = tokio::spawn(read_to_end(stdout));
    let stderr_task = tokio::spawn(read_to_end(stderr));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(RipwireFailure::Spawn {
                message: format!("failed to wait for ripwire: {error}"),
            });
        }
        Err(_) => {
            let _ = child.kill().await;
            // kill() reaps, so wait() returns immediately; this also lets the
            // pipe readers observe EOF instead of blocking forever.
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(RipwireFailure::Timeout { timeout });
        }
    };

    let stdout = join_reader(stdout_task).await;
    let stderr = join_reader(stderr_task).await;
    if status.success() {
        Ok(RipwireOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr,
        })
    } else {
        Err(RipwireFailure::Exit {
            code: status.code(),
            stderr,
        })
    }
}

async fn read_to_end(mut reader: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buffer).await;
    buffer
}

async fn join_reader(task: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    match task.await {
        Ok(bytes) => bytes,
        // A killed child closes its pipes; an aborted reader contributes
        // nothing rather than failing the whole call.
        Err(error) if error.is_cancelled() => Vec::new(),
        Err(_) => Vec::new(),
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

/// Resolve candidates for [`find_ripwire_binary`] from explicit env values so
/// discovery is unit-testable without mutating the process environment.
///
/// Order: explicit `RIPWIRE_PATH`, `$HOME/.local/bin/ripwire`, then every
/// `PATH` entry. There is no hardcoded home directory: a fixed `/root/...`
/// candidate wrong on every non-root install and on root installs with a
/// different `$HOME`.
fn ripwire_candidates(ripwire_path: Option<&OsStr>, home: Option<&OsStr>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(custom) = ripwire_path {
        candidates.push(PathBuf::from(custom));
    }
    if let Some(home) = home {
        candidates.push(PathBuf::from(home).join(".local/bin/ripwire"));
    }
    candidates
}

/// Whether `path` names a regular file the current user may execute.
///
/// Discovery must not accept a readable-but-not-executable `ripwire`: spawning
/// it would fail with `EACCES` and turn every `map` into a spawn error.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Pure discovery core: every environment input is a parameter so the search
/// order and the executability rule are testable without touching the process
/// environment (which edition 2024 only allows through `unsafe`).
fn resolve_ripwire_binary(
    ripwire_path: Option<&OsStr>,
    home: Option<&OsStr>,
    path_var: Option<&OsStr>,
) -> Option<PathBuf> {
    for candidate in ripwire_candidates(ripwire_path, home) {
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    for dir in std::env::split_paths(path_var?) {
        let candidate = dir.join("ripwire");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn find_ripwire_binary() -> Option<PathBuf> {
    let ripwire_path = std::env::var_os("RIPWIRE_PATH");
    let home = std::env::var_os("HOME");
    let path_var = std::env::var_os("PATH");
    resolve_ripwire_binary(
        ripwire_path.as_deref(),
        home.as_deref(),
        path_var.as_deref(),
    )
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

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

    #[cfg(unix)]
    fn executable_script(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn discovery_requires_the_executable_bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let plain = dir.path().join("ripwire");
        fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&plain).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&plain, perms.clone()).unwrap();

        assert!(!is_executable_file(&plain), "mode 0644 is not executable");
        assert!(
            resolve_ripwire_binary(Some(plain.as_os_str()), None, None).is_none(),
            "a readable but non-executable RIPWIRE_PATH must be rejected"
        );

        perms.set_mode(0o755);
        fs::set_permissions(&plain, perms).unwrap();
        assert!(is_executable_file(&plain));
        assert_eq!(
            resolve_ripwire_binary(Some(plain.as_os_str()), None, None),
            Some(plain.clone())
        );
        assert!(
            resolve_ripwire_binary(Some(dir.path().as_os_str()), None, None).is_none(),
            "a directory is not an executable file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_order_and_no_hardcoded_home() {
        let root = tempfile::TempDir::new().unwrap();
        let custom = executable_script(root.path(), "custom-ripwire");
        let home = root.path().join("home");
        let home_bin = home.join(".local/bin");
        fs::create_dir_all(&home_bin).unwrap();
        let home_ripwire = executable_script(&home_bin, "ripwire");
        let on_path = root.path().join("pathdir");
        fs::create_dir_all(&on_path).unwrap();
        let path_ripwire = executable_script(&on_path, "ripwire");

        // RIPWIRE_PATH wins over $HOME and PATH.
        assert_eq!(
            resolve_ripwire_binary(
                Some(custom.as_os_str()),
                Some(home.as_os_str()),
                Some(on_path.as_os_str())
            ),
            Some(custom.clone())
        );
        // $HOME/.local/bin is second.
        assert_eq!(
            resolve_ripwire_binary(None, Some(home.as_os_str()), Some(on_path.as_os_str())),
            Some(home_ripwire.clone())
        );
        // PATH is the last resort.
        assert_eq!(
            resolve_ripwire_binary(None, None, Some(on_path.as_os_str())),
            Some(path_ripwire.clone())
        );
        // A missing HOME is skipped, not replaced by a hardcoded one.
        assert_eq!(
            resolve_ripwire_binary(None, None, None),
            None,
            "no inputs must not resolve a hardcoded /root path"
        );

        // The old hardcoded candidate must not be reachable through HOME when
        // HOME points elsewhere: only the given home is searched.
        let other_home = root.path().join("other-home");
        fs::create_dir_all(&other_home).unwrap();
        assert_eq!(
            resolve_ripwire_binary(
                None,
                Some(other_home.as_os_str()),
                Some(on_path.as_os_str())
            ),
            Some(path_ripwire),
            "an unrelated HOME falls through to PATH"
        );
        // Unset HOME and PATH find nothing even though /root/.local/bin/ripwire
        // exists on some machines: that path is no longer a candidate.
        assert_eq!(
            resolve_ripwire_binary(None, None, Some(OsStr::new(""))),
            None
        );
    }

    #[test]
    fn invocation_flags_follow_the_measured_mode_contract() {
        let target = Path::new("/tmp/project");

        let default_map = RipwireInvocation::new(target, "", 1200, 50);
        assert_eq!(
            default_map.args,
            vec![
                "/tmp/project".to_string(),
                "--max-tokens=1200".to_string(),
                "--top-k=50".to_string()
            ]
        );

        let task_lens = RipwireInvocation::new(target, "server start", 1200, 50);
        assert_eq!(
            task_lens.args,
            vec![
                "/tmp/project".to_string(),
                "--for=server start".to_string(),
                "--token-budget=1200".to_string()
            ]
        );
        // ripwire 0.6.8 ignores both flags under --for and warns on stderr.
        assert!(
            !task_lens.args.iter().any(|arg| arg.starts_with("--top-k")),
            "--top-k is not read by --for"
        );
        assert!(
            !task_lens
                .args
                .iter()
                .any(|arg| arg.starts_with("--max-tokens")),
            "--max-tokens is not read by --for"
        );
    }

    #[test]
    fn timeout_override_rejects_zero_and_garbage() {
        assert_eq!(
            ripwire_timeout_ms(None),
            DEFAULT_RIPWIRE_TIMEOUT_MS,
            "unset env uses the default"
        );
        assert_eq!(
            ripwire_timeout_ms(Some(OsStr::new("250"))),
            250,
            "a numeric override is honoured"
        );
        assert_eq!(
            ripwire_timeout_ms(Some(OsStr::new("0"))),
            DEFAULT_RIPWIRE_TIMEOUT_MS,
            "zero must not disable the guard"
        );
        assert_eq!(
            ripwire_timeout_ms(Some(OsStr::new("soon"))),
            DEFAULT_RIPWIRE_TIMEOUT_MS,
            "garbage falls back instead of failing"
        );
    }

    #[test]
    fn stderr_is_trimmed_and_bounded() {
        assert_eq!(trimmed_stderr(b""), None);
        assert_eq!(trimmed_stderr(b"   \n\t"), None, "whitespace is not text");
        assert_eq!(
            trimmed_stderr(b" warning: nope \n"),
            Some("warning: nope".to_string())
        );

        let long = format!("{}END", "x".repeat(RIPWIRE_STDERR_LIMIT_BYTES));
        let trimmed = trimmed_stderr(long.as_bytes()).unwrap();
        assert!(
            trimmed.len() <= RIPWIRE_STDERR_LIMIT_BYTES + '…'.len_utf8(),
            "trimmed stderr stays bounded, got {} bytes",
            trimmed.len()
        );
        assert!(trimmed.ends_with('…'), "a cut is marked: {trimmed}");
        assert!(!trimmed.contains("END"), "the tail is dropped");

        // Multi-byte input must not panic on a non-char-boundary cut.
        let unicode = "é".repeat(RIPWIRE_STDERR_LIMIT_BYTES);
        assert!(trimmed_stderr(unicode.as_bytes()).is_some());
    }
}
