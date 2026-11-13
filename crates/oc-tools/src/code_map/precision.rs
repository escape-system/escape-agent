//! Precision overlay bridge combining SCIP index discovery (`ripwire --scip`)
//! with an optional host-provided semantic oracle (`oc-lsp`).

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::files::relative_to;
use super::native_index::locate_symbol_line_col;
use crate::{ToolContext, ToolError};

/// Environment variable pointing to an explicit `.scip` index file.
pub const RIPWIRE_SCIP_ENV: &str = "RIPWIRE_SCIP_PATH";

/// Precision level requested for `code_map` graph operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrecisionMode {
    #[default]
    Fast,
    Scip,
    Lsp,
    Hybrid,
}

impl PrecisionMode {
    /// Parse a user-supplied `precision` parameter.
    pub fn parse(raw: Option<&str>) -> Result<Self, ToolError> {
        let Some(val) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self::Fast);
        };
        match val.to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "scip" => Ok(Self::Scip),
            "lsp" => Ok(Self::Lsp),
            "hybrid" => Ok(Self::Hybrid),
            other => Err(ToolError::new(format!(
                "Invalid `precision` '{other}'. Expected 'fast', 'scip', 'lsp', or 'hybrid'."
            ))),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Scip => "scip",
            Self::Lsp => "lsp",
            Self::Hybrid => "hybrid",
        }
    }

    #[must_use]
    pub const fn uses_scip(self) -> bool {
        matches!(self, Self::Scip | Self::Hybrid)
    }

    #[must_use]
    pub const fn uses_lsp(self) -> bool {
        matches!(self, Self::Lsp | Self::Hybrid)
    }
}

/// Compiler-resolved semantic lookup dispatched through an attached host oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticRequest {
    Definition {
        file_path: String,
        line: u32,
        character: u32,
    },
    References {
        file_path: String,
        line: u32,
        character: u32,
    },
    Implementations {
        file_path: String,
        line: u32,
        character: u32,
    },
    IncomingCalls {
        file_path: String,
        line: u32,
        character: u32,
    },
    OutgoingCalls {
        file_path: String,
        line: u32,
        character: u32,
    },
    WorkspaceSymbol {
        query: String,
    },
    Diagnostics {
        file_path: String,
    },
}

/// Host-supplied compiler semantic bridge (typically backed by `oc-lsp`).
#[async_trait::async_trait]
pub trait SemanticPrecisionOracle: Send + Sync {
    async fn query(&self, req: SemanticRequest) -> Result<Value, String>;
}

tokio::task_local! {
    static ACTIVE_ORACLE: Arc<dyn SemanticPrecisionOracle>;
}

/// Run an async tool future with an optional [`SemanticPrecisionOracle`] bound
/// in task-local storage so [`crate::CodeMapTool`] can consult live LSP state
/// without changing [`ToolContext`].
pub async fn with_semantic_oracle<F, T>(
    oracle: Option<Arc<dyn SemanticPrecisionOracle>>,
    fut: F,
) -> T
where
    F: Future<Output = T>,
{
    match oracle {
        Some(oracle) => ACTIVE_ORACLE.scope(oracle, fut).await,
        None => fut.await,
    }
}

#[must_use]
pub(super) fn current_semantic_oracle() -> Option<Arc<dyn SemanticPrecisionOracle>> {
    ACTIVE_ORACLE.try_with(Arc::clone).ok()
}

/// Locate a usable `.scip` index for `root` (explicit env override first, then
/// standard workspace locations).
#[must_use]
pub(super) fn discover_scip_index(root: &Path) -> Option<PathBuf> {
    if let Some(env_path) = env::var_os(RIPWIRE_SCIP_ENV)
        && !env_path.is_empty()
    {
        let p = PathBuf::from(env_path);
        if is_usable_scip_file(&p) {
            return Some(p);
        }
    }
    for rel in ["index.scip", ".ripwire/index.scip", "target/index.scip"] {
        let candidate = root.join(rel);
        if is_usable_scip_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_usable_scip_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

/// Resolve SCIP index path when requested (`Scip` / `Hybrid`) or return `None`
/// for `Fast` / `Lsp` so standard warm daemons are reused untouched.
pub(super) fn scip_for_mode(root: &Path, mode: PrecisionMode) -> Option<PathBuf> {
    if mode.uses_scip() {
        discover_scip_index(root)
    } else {
        None
    }
}

/// Augment `code_map` output with compiler-grade facts from the active
/// [`SemanticPrecisionOracle`] when `precision` is `lsp` or `hybrid`.
pub(super) async fn append_lsp_precision(
    ctx: &ToolContext,
    target_path: &Path,
    operation: &str,
    query: &str,
    mode: PrecisionMode,
    out: &mut String,
    metadata: &mut Value,
) {
    if !mode.uses_lsp() {
        return;
    }

    let Some(oracle) = current_semantic_oracle() else {
        metadata["precision"] = json!(mode.as_str());
        metadata["lsp_precision"] = json!("unavailable");
        if mode == PrecisionMode::Lsp {
            let _ = writeln!(
                out,
                "\n[LSP precision unavailable: no language server oracle attached; showing graph output]"
            );
        }
        return;
    };

    let Some((abs_file, line, character)) = locate_symbol_line_col(ctx, target_path, query) else {
        if let Ok(ws_val) = oracle
            .query(SemanticRequest::WorkspaceSymbol {
                query: query.to_string(),
            })
            .await
        {
            let lines = extract_lsp_locations(&ctx.directory, &ws_val, 15);
            if !lines.is_empty() {
                let _ = writeln!(
                    out,
                    "\n<lsp_precision op=\"{operation}\" via=\"workspaceSymbol\">"
                );
                for row in &lines {
                    let _ = writeln!(out, "  {row}");
                }
                let _ = writeln!(out, "</lsp_precision>");
                metadata["lsp_precision"] = json!("ok");
                metadata["lsp_hits"] = json!(lines.len());
                return;
            }
        }
        metadata["lsp_precision"] = json!("anchor_not_found");
        return;
    };

    let file_str = abs_file.to_string_lossy().into_owned();
    let rel = relative_to(&ctx.directory, &abs_file);
    let mut rows = query_lsp_for_op(
        oracle.as_ref(),
        &ctx.directory,
        operation,
        &file_str,
        line,
        character,
    )
    .await;
    append_lsp_diagnostics(oracle.as_ref(), &rel, &file_str, line, &mut rows).await;

    metadata["precision"] = json!(mode.as_str());
    metadata["lsp_anchor"] = json!(format!("{rel}:{line}:{character}"));
    metadata["lsp_hits"] = json!(rows.len());

    if rows.is_empty() {
        metadata["lsp_precision"] = json!("empty");
    } else {
        metadata["lsp_precision"] = json!("ok");
        let _ = writeln!(
            out,
            "\n<lsp_precision op=\"{operation}\" anchor=\"{rel}:{line}:{character}\">"
        );
        for row in &rows {
            let _ = writeln!(out, "  {row}");
        }
        let _ = writeln!(out, "</lsp_precision>");
    }
}

async fn query_lsp_for_op(
    oracle: &dyn SemanticPrecisionOracle,
    base: &Path,
    operation: &str,
    file_str: &str,
    line: u32,
    character: u32,
) -> BTreeSet<String> {
    let mut rows = BTreeSet::new();
    match operation {
        "lego" => {
            if let Ok(val) = oracle
                .query(SemanticRequest::Implementations {
                    file_path: file_str.to_string(),
                    line,
                    character,
                })
                .await
            {
                for row in extract_lsp_locations(base, &val, 25) {
                    rows.insert(format!("[LSP impl] {row}"));
                }
            }
        }
        "references" => {
            if let Ok(val) = oracle
                .query(SemanticRequest::References {
                    file_path: file_str.to_string(),
                    line,
                    character,
                })
                .await
            {
                for row in extract_lsp_locations(base, &val, 25) {
                    rows.insert(format!("[LSP ref] {row}"));
                }
            }
        }
        "impact" => {
            if let Ok(val) = oracle
                .query(SemanticRequest::IncomingCalls {
                    file_path: file_str.to_string(),
                    line,
                    character,
                })
                .await
            {
                for row in extract_call_hierarchy_rows(base, "caller", &val, 20) {
                    rows.insert(row);
                }
            }
            if rows.is_empty()
                && let Ok(val) = oracle
                    .query(SemanticRequest::References {
                        file_path: file_str.to_string(),
                        line,
                        character,
                    })
                    .await
            {
                for row in extract_lsp_locations(base, &val, 20) {
                    rows.insert(format!("[LSP ref] {row}"));
                }
            }
        }
        _ => {
            if let Ok(val) = oracle
                .query(SemanticRequest::Definition {
                    file_path: file_str.to_string(),
                    line,
                    character,
                })
                .await
            {
                for row in extract_lsp_locations(base, &val, 5) {
                    rows.insert(format!("[LSP def] {row}"));
                }
            }
            if let Ok(val) = oracle
                .query(SemanticRequest::IncomingCalls {
                    file_path: file_str.to_string(),
                    line,
                    character,
                })
                .await
            {
                for row in extract_call_hierarchy_rows(base, "caller", &val, 10) {
                    rows.insert(row);
                }
            }
            if let Ok(val) = oracle
                .query(SemanticRequest::OutgoingCalls {
                    file_path: file_str.to_string(),
                    line,
                    character,
                })
                .await
            {
                for row in extract_call_hierarchy_rows(base, "callee", &val, 10) {
                    rows.insert(row);
                }
            }
        }
    }
    rows
}

async fn append_lsp_diagnostics(
    oracle: &dyn SemanticPrecisionOracle,
    rel: &str,
    file_str: &str,
    fallback_line: u32,
    rows: &mut BTreeSet<String>,
) {
    if let Ok(diag_val) = oracle
        .query(SemanticRequest::Diagnostics {
            file_path: file_str.to_string(),
        })
        .await
        && let Some(items) = diag_val.as_array()
    {
        for item in items.iter().take(5) {
            if item.get("severity").and_then(Value::as_i64) == Some(1)
                && let Some(msg) = item.get("message").and_then(Value::as_str)
            {
                let dline = item
                    .pointer("/range/start/line")
                    .and_then(Value::as_u64)
                    .map_or(fallback_line, |l| u32::try_from(l + 1).unwrap_or(u32::MAX));
                rows.insert(format!("[LSP error] {rel}:{dline} {}", msg.trim()));
            }
        }
    }
}

fn extract_lsp_locations(base: &Path, val: &Value, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    let arr = if let Some(items) = val.as_array() {
        items.as_slice()
    } else if let Some(items) = val.get("symbols").and_then(Value::as_array) {
        items.as_slice()
    } else {
        std::slice::from_ref(val)
    };
    for item in arr {
        if out.len() >= cap {
            break;
        }
        let loc = item.get("location").unwrap_or(item);
        let uri = loc
            .get("uri")
            .or_else(|| loc.get("targetUri"))
            .and_then(Value::as_str);
        let range = loc
            .get("range")
            .or_else(|| loc.get("targetSelectionRange"))
            .or_else(|| loc.get("targetRange"));
        let Some(uri_str) = uri else {
            continue;
        };
        let path_str = uri_to_display_path(base, uri_str);
        let line = range
            .and_then(|r| r.pointer("/start/line"))
            .and_then(Value::as_u64)
            .map_or(1, |l| l.saturating_add(1));
        let label = item
            .get("name")
            .and_then(Value::as_str)
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        out.push(format!("{path_str}:{line}{label}"));
    }
    out
}

fn extract_call_hierarchy_rows(base: &Path, role: &str, val: &Value, cap: usize) -> Vec<String> {
    let Some(items) = val.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in items.iter().take(cap) {
        let node = entry
            .get("from")
            .or_else(|| entry.get("to"))
            .unwrap_or(entry);
        let name = node.get("name").and_then(Value::as_str).unwrap_or("symbol");
        let uri = node.get("uri").and_then(Value::as_str).unwrap_or("");
        let line = node
            .pointer("/selectionRange/start/line")
            .or_else(|| node.pointer("/range/start/line"))
            .and_then(Value::as_u64)
            .map_or(1, |l| l.saturating_add(1));
        let path_str = if uri.is_empty() {
            String::new()
        } else {
            format!(" ({}:{line})", uri_to_display_path(base, uri))
        };
        out.push(format!("[LSP {role}] {name}{path_str}"));
    }
    out
}

fn uri_to_display_path(base: &Path, uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let decoded = percent_decode(raw);
    relative_to(base, Path::new(&decoded))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
