//! Built-in tools: `read`, `write`, `edit`, `patch`, `glob`, `grep`, `shell`,
//! `todowrite` and `webfetch`.
//!
//! The tool layer never performs permission checks. Paths may be absolute or
//! relative; relative paths resolve against [`ToolContext::directory`] (the
//! active location) while `shell` defaults its working directory to
//! [`ToolContext::cwd`]. Absolute paths outside the location are accepted: the
//! runtime is responsible for authorising them before dispatch. `..` segments
//! are resolved normally and are not rejected here.

// `Tool::name` / `Tool::description` return `&str` rather than `&'static str`
// because dynamically discovered tools (MCP) back them with owned `String`s.
// The lint only sees the built-ins' literal returns, so it is a false positive.
#![allow(clippy::unnecessary_literal_bound)]

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use serde::Serialize;
use serde_json::Value;

pub mod code_map;
pub mod diff;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod patch;
pub mod read;
pub mod shell;
pub mod shell_guard;
pub mod symbol_edit;
pub mod todowrite;
pub mod util;
pub mod webfetch;
pub mod websearch;
pub mod write;

pub use code_map::CodeMapTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use patch::PatchTool;
pub use read::ReadTool;
pub use shell::ShellTool;
pub use symbol_edit::SymbolEditTool;
pub use todowrite::TodowriteTool;
pub use webfetch::WebfetchTool;
pub use websearch::WebsearchTool;
pub use write::WriteTool;

/// Execution context for a single tool call.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Session that owns the call; exported to `shell` as `OPENCODE_SESSION_ID`.
    pub session_id: String,
    /// Active location root. Relative file-tool paths resolve here.
    pub directory: PathBuf,
    /// Current working directory; `shell` uses this when no `workdir` is given.
    pub cwd: PathBuf,
    /// Cancelled when the owning execution is interrupted. A tool that spawns
    /// detached helpers (the subagent wait does) uses it to stop them instead
    /// of orphaning work when the caller drops the tool future.
    pub cancel: Option<CancellationToken>,
}

/// Result of a successful tool call.
///
/// `content` is the model-facing content array when a tool needs something
/// other than plain text (media). When absent the runtime synthesises
/// `[{"type":"text","text":output}]`.
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub output: Option<String>,
    pub content: Option<Value>,
    pub metadata: Option<Value>,
}

impl ToolResult {
    pub fn new(output: Option<String>, content: Option<Value>, metadata: Option<Value>) -> Self {
        Self {
            output,
            content,
            metadata,
        }
    }

    pub fn text(output: impl Into<String>) -> Self {
        Self {
            output: Some(output.into()),
            content: None,
            metadata: None,
        }
    }

    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Tool failure surfaced to the model as `Tool.Error`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ToolError {
    pub message: String,
    pub metadata: Option<Value>,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            metadata: None,
        }
    }

    #[must_use]
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A tool callable by the runtime.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Name used as the registry key and in the model-facing definition.
    ///
    /// Borrowed rather than `&'static str` so dynamically discovered tools
    /// (for example MCP tool names) can back it with a `String`.
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the tool input. The runtime injects `expectation`.
    fn schema(&self) -> Value;
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError>;
}

/// Static tool definition exposed to the model.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Registry of built-in tools. Definitions are returned sorted by name, which
/// keeps the request payload stable (and therefore the prompt cache key).
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the built-in tools.
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(CodeMapTool));
        registry.register(Arc::new(PatchTool));
        registry.register(Arc::new(EditTool));
        registry.register(Arc::new(GlobTool));
        registry.register(Arc::new(GrepTool));
        registry.register(Arc::new(ReadTool));
        registry.register(Arc::new(ShellTool));
        registry.register(Arc::new(SymbolEditTool));
        registry.register(Arc::new(TodowriteTool));
        registry.register(Arc::new(WebfetchTool));
        registry.register(Arc::new(WebsearchTool));
        registry.register(Arc::new(WriteTool));
        registry
    }

    /// Insert (or replace) a tool by its declared name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Option<Arc<dyn Tool>> {
        self.tools.insert(tool.name().to_string(), tool)
    }

    /// Clone all tools from `other` into `self`; later registrations replace by name.
    pub fn extend(&mut self, other: &ToolRegistry) {
        for (name, tool) in &other.tools {
            self.tools.insert(name.clone(), Arc::clone(tool));
        }
    }

    /// A new registry containing `self` then `other` (other wins on conflicts).
    #[must_use]
    pub fn merged(&self, other: &ToolRegistry) -> ToolRegistry {
        let mut merged = ToolRegistry {
            tools: self.tools.clone(),
        };
        merged.extend(other);
        merged
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Sorted definitions, ready to be placed in a model request.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "fake tool for registry tests"
        }

        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _ctx: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::default())
        }
    }

    /// Name and description are owned strings, as MCP-discovered tools need.
    #[derive(Debug)]
    struct DynamicTool {
        name: String,
        description: String,
    }

    #[async_trait::async_trait]
    impl Tool for DynamicTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn schema(&self) -> Value {
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            })
        }

        async fn execute(
            &self,
            _ctx: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::default())
        }
    }

    #[test]
    fn non_static_names_register_and_show_up_in_definitions() {
        let (name, description) = (
            "mcp__server__lookup".to_string(),
            "Look something up on the MCP server".to_string(),
        );
        let mut registry = ToolRegistry::new();
        assert!(
            registry
                .register(Arc::new(DynamicTool {
                    name: name.clone(),
                    description: description.clone(),
                }))
                .is_none()
        );

        assert_eq!(registry.names(), vec![name.as_str()]);
        assert!(registry.contains(&name));
        assert_eq!(
            registry.get(&name).map(|tool| tool.name().to_string()),
            Some(name.clone())
        );

        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, name);
        assert_eq!(definitions[0].description, description);
        assert_eq!(
            definitions[0].parameters["properties"]["query"]["type"],
            "string"
        );

        // Re-registering replaces by the same dynamic key rather than duplicating.
        registry.register(Arc::new(DynamicTool {
            name: name.clone(),
            description: "updated".to_string(),
        }));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.definitions()[0].description, "updated");
    }

    #[test]
    fn extend_and_merge_memory_registry_without_duplicates() {
        let mut base = ToolRegistry::with_defaults();
        let mut memory = ToolRegistry::new();
        memory.register(Arc::new(FakeTool("memory_write")));

        base.extend(&memory);
        assert_eq!(base.len(), 13);
        assert!(base.contains("memory_write"));
        assert_eq!(
            base.names()
                .iter()
                .filter(|name| **name == "memory_write")
                .count(),
            1
        );

        let names = base.names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "registry names stay sorted");

        let merged = ToolRegistry::with_defaults().merged(&memory);
        assert_eq!(merged.len(), 13);
        assert!(merged.contains("memory_write"));
        assert_eq!(
            merged
                .names()
                .iter()
                .filter(|name| **name == "memory_write")
                .count(),
            1
        );
    }
}
