//! Deterministic stdio MCP server (`opencode mcp`) backed by `rmcp`.
//!
//! Implements the Road C inverted-control pattern (`docs/30-tos-safe-harness.md`):
//! human-driven IDE or official desktop sessions connect to this server over
//! stdio rather than letting an autonomous loop drive consumer binaries.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use oc_tools::sandbox::{ShellSandboxMode, pre_tool_use_secret_guard, redact_sensitive_output};
use oc_tools::shell::{ShellRunConfig, with_shell_config};
use oc_tools::{ToolContext, ToolRegistry};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer, serve_server};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{Map, Value};

use crate::cli::McpOptions;

const READ_ONLY_TOOLS: &[&str] = &["read", "glob", "grep", "code_map"];
const READ_WRITE_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "code_map",
    "edit",
    "patch",
    "symbol_edit",
    "write",
    "shell",
];

/// Filtered, workspace-confined stdio MCP server backed by [`ToolRegistry`].
#[derive(Clone, Debug)]
pub(crate) struct OpencodeMcpServer {
    workspace_root: PathBuf,
    workspace_canonical: PathBuf,
    registry: Arc<ToolRegistry>,
    read_only: bool,
    sandbox_mode: ShellSandboxMode,
}

impl OpencodeMcpServer {
    /// Construct a new server confined to `workspace_root`.
    pub(crate) fn new(
        workspace_root: PathBuf,
        read_only: bool,
        sandbox_mode: ShellSandboxMode,
    ) -> Self {
        let workspace_canonical = std::fs::canonicalize(&workspace_root)
            .unwrap_or_else(|_| lexical_normalize(&workspace_root));
        let allowed: BTreeSet<&str> = if read_only {
            READ_ONLY_TOOLS.iter().copied().collect()
        } else {
            READ_WRITE_TOOLS.iter().copied().collect()
        };
        let defaults = ToolRegistry::with_defaults();
        let mut filtered = ToolRegistry::new();
        for name in defaults.names() {
            if allowed.contains(name)
                && let Some(tool) = defaults.get(name)
            {
                filtered.register(Arc::clone(tool));
            }
        }
        Self {
            workspace_root,
            workspace_canonical,
            registry: Arc::new(filtered),
            read_only,
            sandbox_mode,
        }
    }

    fn mcp_tools(&self) -> Vec<Tool> {
        self.registry
            .definitions()
            .into_iter()
            .map(|def| {
                let schema_obj: JsonObject = def
                    .parameters
                    .as_object()
                    .cloned()
                    .unwrap_or_else(JsonObject::new);
                let is_read_only = READ_ONLY_TOOLS.contains(&def.name.as_str());
                let annotations = ToolAnnotations::new()
                    .read_only(is_read_only)
                    .destructive(!is_read_only)
                    .idempotent(is_read_only)
                    .open_world(false);
                Tool::new(def.name, def.description, Arc::new(schema_obj))
                    .with_annotations(annotations)
            })
            .collect()
    }

    async fn execute_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
    ) -> Result<CallToolResult, McpError> {
        let Some(tool) = self.registry.get(name).cloned() else {
            return Err(McpError::invalid_params(
                format!("tool `{name}` is not enabled in this MCP session"),
                None,
            ));
        };

        if let Err(reason) = pre_tool_use_secret_guard(name, &args) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(reason)]));
        }

        if let Err(reason) = self.validate_workspace_args(name, &args) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(reason)]));
        }

        let ctx = ToolContext {
            session_id: "mcp_stdio".to_owned(),
            directory: self.workspace_root.clone(),
            cwd: self.workspace_root.clone(),
            cancel: None,
        };
        let shell_cfg = ShellRunConfig {
            sandbox: self.sandbox_mode,
            scrub_secrets: true,
            ..Default::default()
        };

        let result = with_shell_config(shell_cfg, tool.execute(&ctx, Value::Object(args))).await;
        match result {
            Ok(out) => {
                let text = out.output.unwrap_or_else(|| {
                    out.content
                        .map_or_else(|| "ok".to_owned(), |value| value.to_string())
                });
                let scrubbed = redact_sensitive_output(&text).unwrap_or(text);
                Ok(CallToolResult::success(vec![ContentBlock::text(scrubbed)]))
            }
            Err(err) => {
                let scrubbed = redact_sensitive_output(&err.message).unwrap_or(err.message);
                Ok(CallToolResult::error(vec![ContentBlock::text(scrubbed)]))
            }
        }
    }

    fn validate_workspace_args(&self, tool: &str, args: &Map<String, Value>) -> Result<(), String> {
        for field in ["path", "workdir"] {
            if let Some(raw) = args.get(field).and_then(Value::as_str)
                && !raw.trim().is_empty()
            {
                self.ensure_inside_workspace(raw, tool, field)?;
            }
        }
        if tool == "patch"
            && let Some(patch_text) = args.get("patchText").and_then(Value::as_str)
        {
            for line in patch_text.lines() {
                for prefix in [
                    "*** Update File: ",
                    "*** Add File: ",
                    "*** Delete File: ",
                    "*** Move to: ",
                ] {
                    if let Some(target) = line.strip_prefix(prefix) {
                        self.ensure_inside_workspace(target.trim(), tool, "patchText")?;
                    }
                }
            }
        }
        Ok(())
    }

    fn ensure_inside_workspace(&self, raw: &str, tool: &str, field: &str) -> Result<(), String> {
        let joined = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.workspace_root.join(raw)
        };
        let resolved = resolve_with_existing_ancestor(&joined);
        if resolved.starts_with(&self.workspace_canonical) {
            Ok(())
        } else {
            Err(format!(
                "Blocked by MCP workspace confinement: `{tool}` `{field}` path resolves outside workspace root `{}`.",
                self.workspace_canonical.display()
            ))
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn resolve_with_existing_ancestor(path: &Path) -> PathBuf {
    let normalized = lexical_normalize(path);
    let mut prefix = normalized.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !prefix.exists() {
        let Some(name) = prefix.file_name() else {
            break;
        };
        tail.push(name.to_os_string());
        let Some(parent) = prefix.parent() else {
            break;
        };
        prefix = parent;
    }
    let mut resolved = std::fs::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    for component in tail.into_iter().rev() {
        resolved.push(component);
    }
    resolved
}

impl ServerHandler for OpencodeMcpServer {
    fn get_info(&self) -> ServerConfig {
        let mode = if self.read_only {
            "read-only"
        } else {
            "read-write (sandboxed)"
        };
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "escape-agent",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(format!(
                "Escape Agent local workspace tools over stdio MCP ({mode}, workspace: {}).",
                self.workspace_root.display()
            ))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.mcp_tools())))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.mcp_tools()
            .into_iter()
            .find(|tool| tool.name.as_ref() == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        self.execute_tool(request.name.as_ref(), args)
            .await
            .map(CallToolResponse::from)
    }
}

/// Serve workspace tools over stdio MCP until the host closes the transport.
pub(crate) async fn run_mcp(opts: McpOptions) -> anyhow::Result<()> {
    let sandbox_mode = ShellSandboxMode::from_config_str(&opts.sandbox).with_context(|| {
        format!(
            "invalid --sandbox value {:?}; expected auto, bwrap, no-net, or off",
            opts.sandbox
        )
    })?;
    let directory = match opts.directory {
        Some(dir) => dir,
        None => std::env::current_dir().context("resolve current directory")?,
    };
    let server = OpencodeMcpServer::new(directory, opts.read_only, sandbox_mode);
    let running = serve_server(server, rmcp::transport::stdio())
        .await
        .context("initialize MCP stdio server")?;
    running
        .waiting()
        .await
        .context("wait on MCP stdio server shutdown")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_mode_exposes_only_safe_inspection_tools() {
        let dir = tempfile::tempdir().unwrap();
        let server = OpencodeMcpServer::new(dir.path().to_path_buf(), true, ShellSandboxMode::Auto);
        let names: Vec<String> = server
            .mcp_tools()
            .into_iter()
            .map(|t| t.name.into_owned())
            .collect();
        assert_eq!(names, vec!["code_map", "glob", "grep", "read"]);
        assert!(server.get_tool("write").is_none());
        assert!(server.get_tool("shell").is_none());
        assert!(server.get_tool("key_health").is_none());
    }

    #[tokio::test]
    async fn mcp_read_executes_inside_workspace_blocks_escapes_and_redacts_secrets() {
        let ws = tempfile::tempdir().unwrap();
        let secret_file = ws.path().join("notes.txt");
        std::fs::write(
            &secret_file,
            "ok line\ntoken=AIzaSyDummySecretValueForMcpRedactionTest123\n",
        )
        .unwrap();

        let server = OpencodeMcpServer::new(ws.path().to_path_buf(), true, ShellSandboxMode::Auto);

        let mut args = Map::new();
        args.insert("path".to_owned(), Value::String("notes.txt".to_owned()));
        let res = server.execute_tool("read", args).await.expect("read ok");
        assert_eq!(res.is_error, Some(false));
        let json = serde_json::to_string(&res.content).unwrap();
        assert!(json.contains("ok line"));
        assert!(!json.contains("AIzaSyDummy"));
        assert!(json.contains("[REDACTED_SECRET]"));

        // Path escape outside workspace is rejected cleanly.
        let mut escape = Map::new();
        escape.insert(
            "path".to_owned(),
            Value::String("../../etc/passwd".to_owned()),
        );
        let blocked = server
            .execute_tool("read", escape)
            .await
            .expect("tool error");
        assert_eq!(blocked.is_error, Some(true));

        // Write tools are invalid_params in --read-only mode.
        let denied = server.execute_tool("write", Map::new()).await;
        assert!(denied.is_err());
    }
}
