//! `code_map` tool: structural code intelligence, file outlines, symbol definitions,
//! and task-relevant code maps.
//!
//! Provides pure-Rust AST-like symbol extraction for Rust, TypeScript, JavaScript,
//! Python, Go, C/C++, and Markdown without external dependencies, and opportunistically
//! delegates to `ripwire` when present on the system.

use std::path::Path;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

mod files;
mod graph_ops;
mod map;
mod native_index;
mod outline;
mod parsers;
mod precision;
mod radar;
mod ripwire;
mod ripwire_mcp;

pub use parsers::parse_symbols;
pub use precision::{
    PrecisionMode, SemanticPrecisionOracle, SemanticRequest, with_semantic_oracle,
};
pub use radar::{
    EditRadarReport, PartitionSlice, PartitionedTask, collect_post_edit_radar, partition_task,
};
pub use ripwire::{
    DEFAULT_RIPWIRE_TIMEOUT_MS, RIPWIRE_INSTALL_CMD, RIPWIRE_TIMEOUT_ENV, is_executable_file,
};

pub(crate) use files::relative_to;
pub(crate) use graph_ops::parse_mcp_json_text;
pub(crate) use native_index::{NativeSymbolLocation, find_native_symbol_locations, tree_root_for};
pub(crate) use ripwire::{configured_ripwire_timeout, find_ripwire_binary};
pub(crate) use ripwire_mcp::{McpCallError, call_mcp_tool_with_scip};

pub const DEFAULT_LIMIT: usize = 50;
pub const DEFAULT_MAX_TOKENS: usize = 1200;

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
        "Graph-backed structural code intelligence and token-budgeted context packing. \
         Prefer over repeated `grep` + whole-file `read` calls: use `pack_task` (with optional \
         `partitions` 2..16 for parallel worker slices) for task-relevant signatures + call \
         neighbors + code slices in one call, `communities` for architectural module clusters, \
         `map` for ranked repo maps, `symbol`/`expand` for definitions and compressed bodies, \
         `impact`/`references`/`connect`/`affected` for blast radius and test impact, `trace` for \
         stack traces, `batch` for up to 16 sub-queries, plus deep lenses `lego` (trait/interface \
         implementors), `slice` (intra-procedural variable flow), `cochange` (git co-edit coupling), \
         `doc_drift` (stale docs), `flags` (feature/env gates), and `quality` (delta/dead_code/\
         clones/hotspots/deps/seams). Supports `precision`: 'fast' | 'scip' | 'lsp' | 'hybrid'."
    }

    fn schema(&self) -> Value {
        code_map_schema()
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let req = CodeMapRequest::parse(ctx, &input)?;
        req.dispatch(ctx).await
    }
}

fn core_schema_properties() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "operation".into(),
        json!({
            "type": "string",
            "enum": [
                "pack_task", "map", "communities", "outline", "symbol", "expand",
                "impact", "references", "connect", "affected", "trace", "batch",
                "lego", "slice", "cochange", "doc_drift", "flags", "quality"
            ],
            "description": "Operation: 'pack_task' (budgeted focal signatures + callers + bodies, optional `partitions`), \
                            'map' (ranked structural map), 'communities' (module clusters & bridges), \
                            'outline' (file/dir signatures), 'symbol' (definition + handle + 1-hop edges), \
                            'expand' (full or sliced body), 'impact' (blast radius dependents), \
                            'references' (callers & literal hits), 'connect' (shortest path `A -> B`), \
                            'affected' (tests impacted by changes), 'trace' (stack trace mapping), \
                            'batch' (up to 16 sub-queries), 'lego' (trait/interface contract & impls), \
                            'slice' (def-use variable slice inside a symbol), 'cochange' (git co-edit partners), \
                            'doc_drift' (broken doc path/symbol anchors), 'flags' (feature/env gates), \
                            'quality' (`delta` regressions, `dead_code`, `clones`, `hotspots`, `deps`, `seams`)."
        }),
    );
    map.insert(
        "path".into(),
        json!({ "type": "string", "description": "File or directory to inspect. Defaults to the working directory." }),
    );
    map.insert(
        "query".into(),
        json!({ "type": "string", "description": "Symbol/type name, file:symbol, @file:line, handle, task description, `A -> B` pair (for connect), CSV seeds (for affected), stack trace (for trace), file path (for cochange), filter (for doc_drift/flags), or mode (for quality: delta|dead_code|clones|hotspots|deps|seams)." }),
    );
    map.insert(
        "limit".into(),
        json!({ "type": "integer", "minimum": 1, "description": "Maximum number of results to return (default: 50)." }),
    );
    map.insert(
        "max_tokens".into(),
        json!({ "type": "integer", "minimum": 100, "description": "Approximate token budget for generated context/map (default: 1200)." }),
    );
    map.insert(
        "rank_by".into(),
        json!({ "type": "string", "enum": ["centrality", "churn", "coupling", "risk"], "description": "Optional ranking lens for 'map': centrality, churn, coupling, or risk." }),
    );
    map
}

fn code_map_schema() -> Value {
    let mut props = core_schema_properties();
    props.insert(
        "start_line".into(),
        json!({ "type": "integer", "minimum": 1, "description": "Optional 1-based start line relative to symbol body for 'expand'." }),
    );
    props.insert(
        "end_line".into(),
        json!({ "type": "integer", "minimum": 1, "description": "Optional 1-based end line relative to symbol body for 'expand'." }),
    );
    props.insert(
        "compress".into(),
        json!({ "type": "boolean", "description": "Strip comment-only lines and collapse blank runs in 'expand' (default: false)." }),
    );
    props.insert(
        "queries".into(),
        json!({
            "type": "array",
            "items": { "type": "string" },
            "minItems": 1,
            "maxItems": 16,
            "description": "List of 1..16 sub-queries for 'batch', formatted as `verb:arg`."
        }),
    );
    props.insert(
        "partitions".into(),
        json!({ "type": "integer", "minimum": 2, "maximum": 16, "description": "For 'pack_task': split task into N disjoint module slices + shared core (2..16)." }),
    );
    props.insert(
        "var".into(),
        json!({ "type": "string", "description": "For 'slice': variable name inside the target symbol (omit to list sliceable locals)." }),
    );
    props.insert(
        "flow".into(),
        json!({ "type": "string", "enum": ["back", "fwd", "both"], "description": "For 'slice': transitive flow direction ('back', 'fwd', or 'both')." }),
    );
    props.insert(
        "depth".into(),
        json!({ "type": "integer", "minimum": 1, "maximum": 32, "description": "For 'slice': BFS flow hop limit in 1..32 (default: 8)." }),
    );
    props.insert(
        "precision".into(),
        json!({ "type": "string", "enum": ["fast", "scip", "lsp", "hybrid"], "description": "Precision bridge: 'fast' (default tree-sitter/native graph), 'scip' (+discovered .scip overlay), 'lsp' (+live oc-lsp compiler facts), or 'hybrid' (SCIP + LSP)." }),
    );
    json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": ["operation"],
        "additionalProperties": false
    })
}

struct CodeMapRequest {
    operation: String,
    target_path: std::path::PathBuf,
    query: Option<String>,
    limit: usize,
    max_tokens: usize,
    rank_by: Option<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    compress: bool,
    queries: Option<Vec<String>>,
    partitions: Option<usize>,
    var: Option<String>,
    flow: Option<String>,
    depth: Option<usize>,
    precision: PrecisionMode,
}

impl CodeMapRequest {
    fn parse(ctx: &ToolContext, input: &Value) -> Result<Self, ToolError> {
        let operation = util::required_string(input, "operation")?;
        let rel_path = util::optional_string(input, "path")?;
        let query = util::optional_string(input, "query")?;
        let limit = util::optional_u64(input, "limit")?.map_or(DEFAULT_LIMIT, util::to_usize);
        let max_tokens =
            util::optional_u64(input, "max_tokens")?.map_or(DEFAULT_MAX_TOKENS, util::to_usize);
        let rank_by = util::optional_string(input, "rank_by")?;
        let start_line = util::optional_u64(input, "start_line")?
            .map(util::to_usize)
            .filter(|v| *v >= 1);
        let end_line = util::optional_u64(input, "end_line")?
            .map(util::to_usize)
            .filter(|v| *v >= 1);
        let compress = util::optional_bool(input, "compress")?.unwrap_or(false);
        let queries = optional_string_list(input, "queries")?;
        let partitions = util::optional_u64(input, "partitions")?.map(util::to_usize);
        let var = util::optional_string(input, "var")?;
        let flow = util::optional_string(input, "flow")?;
        let depth = util::optional_u64(input, "depth")?.map(util::to_usize);
        let precision =
            PrecisionMode::parse(util::optional_string(input, "precision")?.as_deref())?;

        if let Some(p) = partitions
            && !(2..=16).contains(&p)
        {
            return Err(ToolError::new(format!(
                "Invalid `partitions` '{p}'. Expected an integer between 2 and 16."
            )));
        }
        if let Some(d) = depth
            && !(1..=32).contains(&d)
        {
            return Err(ToolError::new(format!(
                "Invalid `depth` '{d}'. Expected an integer between 1 and 32."
            )));
        }
        if let Some(ref f) = flow
            && !matches!(f.trim(), "back" | "fwd" | "both")
        {
            return Err(ToolError::new(format!(
                "Invalid `flow` '{f}'. Expected 'back', 'fwd', or 'both'."
            )));
        }
        if let Some(ref mode) = rank_by
            && !matches!(mode.as_str(), "centrality" | "churn" | "coupling" | "risk")
        {
            return Err(ToolError::new(format!(
                "Invalid `rank_by` '{mode}'. Expected 'centrality', 'churn', 'coupling', or 'risk'."
            )));
        }

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

        Ok(Self {
            operation,
            target_path,
            query,
            limit,
            max_tokens,
            rank_by,
            start_line,
            end_line,
            compress,
            queries,
            partitions,
            var,
            flow,
            depth,
            precision,
        })
    }

    async fn dispatch(self, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        let p = &self.target_path;
        match self.operation.as_str() {
            "pack_task" => {
                let q = require_query(self.query, "pack_task")?;
                CodeMapTool::execute_pack_task(
                    ctx,
                    p,
                    &q,
                    self.max_tokens,
                    self.limit,
                    self.partitions,
                    self.precision,
                )
                .await
            }
            "map" => {
                let q = self.query.as_deref().unwrap_or_default();
                CodeMapTool::execute_map(
                    ctx,
                    p,
                    q,
                    self.rank_by.as_deref(),
                    self.max_tokens,
                    self.limit,
                    self.precision,
                )
                .await
            }
            "communities" => {
                CodeMapTool::execute_communities(
                    ctx,
                    p,
                    self.query.as_deref(),
                    self.max_tokens,
                    self.limit,
                )
                .await
            }
            "outline" => CodeMapTool::execute_outline(ctx, p, self.limit),
            "symbol" => {
                let q = require_query(self.query, "symbol")?;
                CodeMapTool::execute_symbol(ctx, p, &q, self.limit, self.precision).await
            }
            "expand" => {
                let q = require_query(self.query, "expand")?;
                CodeMapTool::execute_expand(
                    ctx,
                    p,
                    &q,
                    self.start_line,
                    self.end_line,
                    self.compress,
                )
                .await
            }
            "impact" => {
                let q = require_query(self.query, "impact")?;
                CodeMapTool::execute_impact(ctx, p, &q, self.limit, self.precision).await
            }
            "references" => {
                let q = require_query(self.query, "references")?;
                CodeMapTool::execute_references(ctx, p, &q, self.limit, self.precision).await
            }
            "connect" => {
                let q = require_query(self.query, "connect")?;
                CodeMapTool::execute_connect(ctx, p, &q, self.precision).await
            }
            "affected" => {
                CodeMapTool::execute_affected(ctx, p, self.query.as_deref(), self.limit).await
            }
            "trace" => {
                let q = require_query(self.query, "trace")?;
                CodeMapTool::execute_trace(ctx, p, &q, self.max_tokens).await
            }
            "batch" => {
                let items = self.queries.ok_or_else(|| {
                    ToolError::new("Parameter `queries` is required for operation 'batch'")
                })?;
                CodeMapTool::execute_batch(
                    ctx,
                    p,
                    &items,
                    self.max_tokens,
                    self.limit,
                    self.compress,
                )
                .await
            }
            _ => self.dispatch_deep(ctx).await,
        }
    }

    async fn dispatch_deep(self, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        let p = &self.target_path;
        match self.operation.as_str() {
            "lego" => {
                let q = require_query(self.query, "lego")?;
                CodeMapTool::execute_lego(ctx, p, &q, self.limit, self.precision).await
            }
            "slice" => {
                let q = require_query(self.query, "slice")?;
                CodeMapTool::execute_slice(
                    ctx,
                    p,
                    &q,
                    self.var.as_deref(),
                    self.flow.as_deref(),
                    self.depth,
                    self.precision,
                )
                .await
            }
            "cochange" => {
                CodeMapTool::execute_cochange(
                    ctx,
                    p,
                    self.query.as_deref(),
                    self.limit,
                    self.precision,
                )
                .await
            }
            "doc_drift" => {
                CodeMapTool::execute_doc_drift(
                    ctx,
                    p,
                    self.query.as_deref(),
                    self.limit,
                    self.precision,
                )
                .await
            }
            "flags" => {
                CodeMapTool::execute_flags(
                    ctx,
                    p,
                    self.query.as_deref(),
                    self.limit,
                    self.precision,
                )
                .await
            }
            "quality" => {
                CodeMapTool::execute_quality(
                    ctx,
                    p,
                    self.query.as_deref(),
                    self.max_tokens,
                    self.limit,
                    self.precision,
                )
                .await
            }
            other => Err(ToolError::new(format!(
                "Unsupported operation '{other}'. Expected one of: pack_task, map, communities, outline, symbol, expand, impact, references, connect, affected, trace, batch, lego, slice, cochange, doc_drift, flags, quality."
            ))),
        }
    }
}

fn require_query(query: Option<String>, op: &str) -> Result<String, ToolError> {
    query
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
        .ok_or_else(|| {
            ToolError::new(format!(
                "Parameter `query` is required for operation '{op}'"
            ))
        })
}

fn optional_string_list(input: &Value, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    let Some(raw) = input.get(key) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let arr = raw.as_array().ok_or_else(|| {
        ToolError::new(format!(
            "Invalid arguments: `{key}` must be an array of strings"
        ))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item.as_str().ok_or_else(|| {
            ToolError::new(format!(
                "Invalid arguments: `{key}` must contain only strings"
            ))
        })?;
        out.push(s.to_string());
    }
    Ok(Some(out))
}
