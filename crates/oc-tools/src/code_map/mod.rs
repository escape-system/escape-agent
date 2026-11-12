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
mod map;
mod outline;
mod parsers;
mod ripwire;

pub use parsers::parse_symbols;
pub use ripwire::{
    DEFAULT_RIPWIRE_TIMEOUT_MS, RIPWIRE_INSTALL_CMD, RIPWIRE_TIMEOUT_ENV, is_executable_file,
};

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
