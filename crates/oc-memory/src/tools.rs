//! The four memory tools exposed to the model.
//!
//! [`MemoryTools::registry`] returns a registry containing only
//! `memory_write`, `memory_search`, `memory_forget` and `memory_list`; the host
//! merges it into its own registry. Scope ids come from the host: the project
//! id is either configured with [`MemoryTools::with_project_id`] or derived
//! from [`ToolContext::directory`] the same way sessions derive it, and the
//! agent id (needed for `scope: "agent"`) must be configured with
//! [`MemoryTools::with_agent_id`]. Session scope uses the calling session.

use std::sync::Arc;

use async_trait::async_trait;
use oc_tools::util;
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::project_id_from_directory;
use serde_json::{Value, json};

use crate::error::MemoryError;
use crate::render::normalize_line;
use crate::service::{
    LIST_DEFAULT, LIST_MAX, ListRequest, MemoryService, NewMemoryInput, RetrieveRequest,
};

const SCOPES: [&str; 4] = ["global", "project", "agent", "session"];
const DEFAULT_SEARCH_LIMIT: usize = 8;

const WRITE_DESCRIPTION: &str = "Store one durable memory that survives sessions: a project fact, decision, preference, \
     pattern or gotcha. Prefer memories a future session cannot derive from the repository. \
     Text is capped at 2000 characters; credentials are rejected. Write at most one memory per \
     turn unless the user asks for more.";

const SEARCH_DESCRIPTION: &str = "Search durable memories visible to this session (global, project, agent and session \
     scopes). Results are rendered as a <memory> block with ids and kinds.";

const FORGET_DESCRIPTION: &str =
    "Soft-delete a memory by id (mem_…). It stops appearing in searches immediately.";

const LIST_DESCRIPTION: &str = "List stored memories by importance and recency, optionally filtered by scope, kind and \
     cursor.";

#[derive(Debug, Clone)]
struct ToolScope {
    service: Arc<MemoryService>,
    project_id: Option<String>,
    agent_id: Option<String>,
}

impl ToolScope {
    fn project_id(&self, ctx: &ToolContext) -> String {
        self.project_id.clone().unwrap_or_else(|| {
            project_id_from_directory(&ctx.directory.to_string_lossy()).into_inner()
        })
    }

    fn agent_id(&self) -> Result<String, MemoryError> {
        self.agent_id.clone().ok_or_else(|| {
            MemoryError::invalid(
                "agent-scoped memory needs the active agent id; construct MemoryTools with \
                 with_agent_id",
            )
        })
    }

    fn scope_id(&self, ctx: &ToolContext, scope: &str) -> Result<Option<String>, MemoryError> {
        match scope {
            "global" => Ok(None),
            "project" => Ok(Some(self.project_id(ctx))),
            "session" => Ok(Some(ctx.session_id.clone())),
            "agent" => Ok(Some(self.agent_id()?)),
            other => Err(MemoryError::invalid(format!(
                "unknown memory scope {other:?}; expected global, project, agent or session"
            ))),
        }
    }

    /// Requested scope, or every scope visible to the call (shared first).
    fn scopes(
        &self,
        ctx: &ToolContext,
        requested: Option<&str>,
    ) -> Result<Vec<(String, Option<String>)>, MemoryError> {
        if let Some(scope) = requested {
            return Ok(vec![(scope.to_owned(), self.scope_id(ctx, scope)?)]);
        }
        let mut scopes = vec![
            ("global".to_owned(), None),
            ("project".to_owned(), Some(self.project_id(ctx))),
        ];
        if let Some(agent) = &self.agent_id {
            scopes.push(("agent".to_owned(), Some(agent.clone())));
        }
        if !ctx.session_id.trim().is_empty() {
            scopes.push(("session".to_owned(), Some(ctx.session_id.clone())));
        }
        Ok(scopes)
    }
}

/// Builds the memory tool registry. Cheap to clone and share across sessions.
#[derive(Debug, Clone)]
pub struct MemoryTools {
    scope: Arc<ToolScope>,
}

impl MemoryTools {
    /// Tools for one memory service.
    #[must_use]
    pub fn new(service: Arc<MemoryService>) -> Self {
        Self {
            scope: Arc::new(ToolScope {
                service,
                project_id: None,
                agent_id: None,
            }),
        }
    }

    /// Pin the project id used for `scope: "project"` (the host usually has a
    /// canonical id from the location service).
    #[must_use]
    pub fn with_project_id(mut self, project_id: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.scope).project_id = Some(project_id.into());
        self
    }

    /// Pin the active agent id so `scope: "agent"` is usable.
    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.scope).agent_id = Some(agent_id.into());
        self
    }

    /// Registry containing exactly the four memory tools.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(MemoryWriteTool {
            scope: Arc::clone(&self.scope),
        }));
        registry.register(Arc::new(MemorySearchTool {
            scope: Arc::clone(&self.scope),
        }));
        registry.register(Arc::new(MemoryForgetTool {
            scope: Arc::clone(&self.scope),
        }));
        registry.register(Arc::new(MemoryListTool {
            scope: Arc::clone(&self.scope),
        }));
        registry
    }
}

impl From<MemoryError> for ToolError {
    fn from(error: MemoryError) -> Self {
        Self::new(error.to_string())
    }
}

fn string_array(input: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(value) => {
                        let value = value.trim();
                        if !value.is_empty() {
                            values.push(value.to_owned());
                        }
                    }
                    _ => {
                        return Err(ToolError::new(format!(
                            "Invalid arguments: `{key}` must be an array of strings"
                        )));
                    }
                }
            }
            Ok(values)
        }
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be an array of strings"
        ))),
    }
}

#[allow(clippy::cast_possible_truncation)] // clamped to 0.0..=1.0 first
fn optional_confidence(input: &Value, key: &str) -> Result<Option<f32>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => {
            let value = number.as_f64().ok_or_else(|| {
                ToolError::new(format!("Invalid arguments: `{key}` must be a number"))
            })?;
            Ok(Some(value.clamp(0.0, 1.0) as f32))
        }
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be a number"
        ))),
    }
}

/// `memory_write {text, kind?, scope?, tags?, importance?, confidence?}`
#[derive(Debug)]
pub struct MemoryWriteTool {
    scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &'static str {
        "memory_write"
    }

    fn description(&self) -> &'static str {
        WRITE_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "Memory text: one self-contained statement, at most 2000 characters"
                },
                "kind": {
                    "type": "string",
                    "enum": ["fact", "decision", "preference", "pattern", "gotcha"],
                    "description": "Memory kind (default: fact)"
                },
                "scope": {
                    "type": "string",
                    "enum": SCOPES,
                    "description": "Where the memory applies (default: project)"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional tags used by search"
                },
                "importance": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 5,
                    "description": "1-5 (default: 3); importance 5 is pinned into every retrieval"
                },
                "confidence": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": 1,
                    "description": "0-1 (default: 0.7)"
                }
            },
            "required": ["text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let text = util::required_string(&input, "text")?;
        let kind = util::optional_string(&input, "kind")?
            .map(|kind| kind.trim().to_owned())
            .filter(|kind| !kind.is_empty());
        let scope = util::optional_string(&input, "scope")?
            .map(|scope| scope.trim().to_owned())
            .filter(|scope| !scope.is_empty())
            .unwrap_or_else(|| "project".to_owned());
        if !SCOPES.contains(&scope.as_str()) {
            return Err(ToolError::new(format!(
                "Invalid arguments: `scope` must be one of {}",
                SCOPES.join(", ")
            )));
        }
        let scope_id = self.scope.scope_id(ctx, &scope).map_err(ToolError::from)?;
        let tags = string_array(&input, "tags")?;
        let importance = util::optional_u64(&input, "importance")?
            .map(|value| u8::try_from(value.clamp(1, 5)).unwrap_or(5));
        let confidence = optional_confidence(&input, "confidence")?;

        let stored = self
            .scope
            .service
            .write(NewMemoryInput {
                text,
                kind,
                scope: Some(scope.clone()),
                scope_id,
                tags,
                importance,
                confidence,
                source: Some("agent".to_owned()),
                source_session: Some(ctx.session_id.clone()),
                source_message: None,
            })
            .await
            .map_err(ToolError::from)?;

        let id = stored["id"].as_str().unwrap_or_default().to_owned();
        let stored_kind = stored["kind"].as_str().unwrap_or("fact").to_owned();
        Ok(ToolResult::new(
            Some(format!("Stored memory {id} [{stored_kind}] scope={scope}")),
            None,
            Some(json!({"id": id, "kind": stored_kind, "scope": scope})),
        ))
    }
}

/// `memory_search {query, scope?, kinds?, tags?, limit?}`
#[derive(Debug)]
pub struct MemorySearchTool {
    scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        SEARCH_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to search for in memory text"
                },
                "scope": {
                    "type": "string",
                    "enum": SCOPES,
                    "description": "Limit to one scope; defaults to every scope visible to this session"
                },
                "kinds": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["fact", "decision", "preference", "pattern", "gotcha"]
                    },
                    "description": "Restrict to these kinds"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Require at least one of these tags"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Maximum number of memories in the block (default: 8)"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let query = util::required_string(&input, "query")?;
        let requested = util::optional_string(&input, "scope")?;
        let kinds = string_array(&input, "kinds")?;
        let tags = string_array(&input, "tags")?;
        let limit = util::optional_u64(&input, "limit")?
            .map_or(DEFAULT_SEARCH_LIMIT, util::to_usize)
            .clamp(1, 50);
        let scopes = self
            .scope
            .scopes(ctx, requested.as_deref())
            .map_err(ToolError::from)?;

        let block = self
            .scope
            .service
            .retrieve(RetrieveRequest {
                scopes,
                query,
                kinds,
                tags,
                limit,
                token_budget: 0,
            })
            .await
            .map_err(ToolError::from)?;

        let output = if block.text.is_empty() {
            "No memories found.".to_owned()
        } else {
            block.text.clone()
        };
        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"count": block.ids.len(), "tokens": block.tokens})),
        ))
    }
}

/// `memory_forget {id}`
#[derive(Debug)]
pub struct MemoryForgetTool {
    scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &'static str {
        "memory_forget"
    }

    fn description(&self) -> &'static str {
        FORGET_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^mem_",
                    "description": "Memory id (mem_…)"
                }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let id = util::required_string(&input, "id")?;
        let forgotten = self
            .scope
            .service
            .forget(&id)
            .await
            .map_err(ToolError::from)?;
        let output = if forgotten {
            format!("Forgot memory {id}.")
        } else {
            format!("No live memory with id {id}.")
        };
        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"id": id, "forgotten": forgotten})),
        ))
    }
}

/// `memory_list {scope?, kind?, limit?, cursor?}`
#[derive(Debug)]
pub struct MemoryListTool {
    scope: Arc<ToolScope>,
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &'static str {
        "memory_list"
    }

    fn description(&self) -> &'static str {
        LIST_DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": SCOPES,
                    "description": "Limit to one scope; defaults to every scope visible to this session"
                },
                "kind": {
                    "type": "string",
                    "enum": ["fact", "decision", "preference", "pattern", "gotcha"],
                    "description": "Restrict to this kind"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Page size (default: 20)"
                },
                "cursor": {
                    "type": "string",
                    "description": "Cursor from `more: cursor=<n>` in a previous page"
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let requested = util::optional_string(&input, "scope")?;
        let kind = util::optional_string(&input, "kind")?
            .map(|kind| kind.trim().to_owned())
            .filter(|kind| !kind.is_empty());
        let limit = util::optional_u64(&input, "limit")?
            .map_or(LIST_DEFAULT, util::to_usize)
            .clamp(1, LIST_MAX);
        let cursor = util::optional_string(&input, "cursor")?;
        let scopes = self
            .scope
            .scopes(ctx, requested.as_deref())
            .map_err(ToolError::from)?;

        let mut rows = self
            .scope
            .service
            .list(ListRequest {
                scopes,
                kind,
                tags: Vec::new(),
                limit: limit.saturating_add(1),
                cursor: cursor.clone(),
            })
            .await
            .map_err(ToolError::from)?;
        let more = rows.len() > limit;
        rows.truncate(limit);

        let count = rows.len();
        let offset = cursor
            .as_deref()
            .and_then(|cursor| cursor.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let next = more.then(|| offset.saturating_add(count).to_string());

        let mut output = String::new();
        for row in &rows {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(row["id"].as_str().unwrap_or_default());
            output.push_str(" [");
            output.push_str(row["kind"].as_str().unwrap_or("fact"));
            output.push_str("] (");
            output.push_str(row["scope"].as_str().unwrap_or("project"));
            output.push_str(") ");
            output.push_str(&normalize_line(row["text"].as_str().unwrap_or_default()));
        }
        if output.is_empty() {
            output.push_str("No memories stored.");
        } else if let Some(next) = &next {
            output.push_str("\n(more: cursor=");
            output.push_str(next);
            output.push(')');
        }

        Ok(ToolResult::new(
            Some(output),
            None,
            Some(json!({"count": count, "cursor": next})),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_store::Store;
    use std::path::PathBuf;

    fn context() -> ToolContext {
        ToolContext {
            session_id: "ses_memory_test".to_owned(),
            directory: PathBuf::from("/projects/memory-test"),
            cwd: PathBuf::from("/projects/memory-test"),
            cancel: None,
        }
    }

    async fn registry() -> (Arc<MemoryService>, ToolRegistry) {
        let service = Arc::new(MemoryService::new(Arc::new(
            Store::open_memory().await.expect("store"),
        )));
        let tools = MemoryTools::new(Arc::clone(&service)).with_project_id("prj_test");
        (service, tools.registry())
    }

    fn tool(registry: &ToolRegistry, name: &str) -> Arc<dyn Tool> {
        Arc::clone(registry.get(name).expect("registered tool"))
    }

    #[tokio::test]
    async fn registry_contains_only_memory_tools() {
        let (_service, registry) = registry().await;
        assert_eq!(
            registry.names(),
            vec![
                "memory_forget",
                "memory_list",
                "memory_search",
                "memory_write"
            ]
        );
    }

    #[tokio::test]
    async fn schemas_do_not_declare_expectation() {
        let (_service, registry) = registry().await;
        for definition in registry.definitions() {
            assert!(
                definition.parameters["properties"]
                    .get("expectation")
                    .is_none(),
                "{} leaks expectation",
                definition.name
            );
            assert_eq!(definition.parameters["additionalProperties"], false);
        }
        let write = registry
            .definitions()
            .into_iter()
            .find(|definition| definition.name == "memory_write")
            .expect("write schema");
        assert_eq!(write.parameters["required"], json!(["text"]));
    }

    #[tokio::test]
    async fn all_four_tools_execute() {
        let (service, registry) = registry().await;
        let ctx = context();

        let written = tool(&registry, "memory_write")
            .execute(
                &ctx,
                json!({
                    "text": "Use rusqlite with a single writer because sqlx compiles too slowly",
                    "kind": "decision",
                    "tags": ["storage"],
                    "importance": 5
                }),
            )
            .await
            .expect("write");
        let written_text = written.output.expect("output");
        assert!(
            written_text.contains("Stored memory mem_"),
            "{written_text}"
        );
        assert!(written_text.contains("[decision]"));
        assert!(written_text.contains("scope=project"));

        let rows = service
            .list(ListRequest {
                scopes: vec![("project".to_owned(), Some("prj_test".to_owned()))],
                ..ListRequest::default()
            })
            .await
            .expect("list");
        let id = rows[0]["id"].as_str().expect("id").to_owned();
        assert!(written_text.contains(&id));

        let searched = tool(&registry, "memory_search")
            .execute(&ctx, json!({"query": "sqlite writer"}))
            .await
            .expect("search");
        let search_text = searched.output.expect("output");
        assert!(
            search_text.starts_with("<memory scope=\"project\">"),
            "{search_text}"
        );
        assert!(search_text.contains(&id), "{search_text}");
        assert!(
            search_text.contains("[decision] Use rusqlite with a single writer"),
            "{search_text}"
        );
        assert_eq!(searched.metadata.expect("metadata")["count"], 1);

        let listed = tool(&registry, "memory_list")
            .execute(&ctx, json!({}))
            .await
            .expect("list");
        let list_text = listed.output.expect("output");
        assert!(list_text.contains(&id), "{list_text}");
        assert!(list_text.contains("[decision]"), "{list_text}");
        assert!(list_text.contains("(project)"), "{list_text}");

        let forgotten = tool(&registry, "memory_forget")
            .execute(&ctx, json!({"id": id}))
            .await
            .expect("forget");
        assert!(forgotten.output.expect("output").contains("Forgot memory"));

        let again = tool(&registry, "memory_forget")
            .execute(&ctx, json!({"id": id}))
            .await
            .expect("forget twice");
        assert!(
            again
                .output
                .expect("output")
                .contains("No live memory with id")
        );
    }

    #[tokio::test]
    async fn search_scope_and_empty_results() {
        let (_service, registry) = registry().await;
        let ctx = context();

        let empty = tool(&registry, "memory_search")
            .execute(&ctx, json!({"query": "nothing matches this"}))
            .await
            .expect("search");
        assert_eq!(empty.output.as_deref(), Some("No memories found."));

        tool(&registry, "memory_write")
            .execute(
                &ctx,
                json!({
                    "text": "Globally prefer small focused commits over large rewrites",
                    "scope": "global",
                    "kind": "preference"
                }),
            )
            .await
            .expect("write global");

        let global_only = tool(&registry, "memory_search")
            .execute(&ctx, json!({"query": "focused commits", "scope": "global"}))
            .await
            .expect("search global");
        assert!(
            global_only
                .output
                .expect("output")
                .contains("small focused commits")
        );

        let project_only = tool(&registry, "memory_search")
            .execute(
                &ctx,
                json!({"query": "focused commits", "scope": "project"}),
            )
            .await
            .expect("search project");
        assert_eq!(project_only.output.as_deref(), Some("No memories found."));
    }

    #[tokio::test]
    async fn tool_errors_are_typed_and_never_echo_secrets() {
        let (_service, registry) = registry().await;
        let ctx = context();
        let error = tool(&registry, "memory_write")
            .execute(
                &ctx,
                json!({"text": "the deploy token is sk-proj-abcdefghijklmnopqrstuvwxyz"}),
            )
            .await
            .unwrap_err();
        assert!(
            error.message.contains("openai-api-key"),
            "{}",
            error.message
        );
        assert!(
            !error.message.contains("sk-proj-abcdefghijklmnopqrstuvwxyz"),
            "{}",
            error.message
        );

        let bad_scope = tool(&registry, "memory_write")
            .execute(
                &ctx,
                json!({"text": "some memory text", "scope": "workspace"}),
            )
            .await
            .unwrap_err();
        assert!(
            bad_scope.message.contains("`scope`"),
            "{}",
            bad_scope.message
        );
    }

    #[tokio::test]
    async fn agent_scope_requires_a_configured_agent() {
        let service = Arc::new(MemoryService::new(Arc::new(
            Store::open_memory().await.expect("store"),
        )));
        let tools = MemoryTools::new(service).with_project_id("prj_test");
        let registry = tools.registry();
        let ctx = context();
        let error = tool(&registry, "memory_write")
            .execute(
                &ctx,
                json!({"text": "agent-scoped memory without an agent id", "scope": "agent"}),
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("with_agent_id"), "{}", error.message);
    }

    #[tokio::test]
    async fn list_pagination_emits_a_cursor() {
        let (_service, registry) = registry().await;
        let ctx = context();
        for index in 0..3 {
            tool(&registry, "memory_write")
                .execute(
                    &ctx,
                    json!({"text": format!("memory number {index} for pagination checks")}),
                )
                .await
                .expect("write");
        }

        let first = tool(&registry, "memory_list")
            .execute(&ctx, json!({"limit": 2}))
            .await
            .expect("list");
        let text = first.output.expect("output");
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(text.ends_with("(more: cursor=2)"), "{text}");
        assert_eq!(first.metadata.expect("metadata")["cursor"], "2");

        let second = tool(&registry, "memory_list")
            .execute(&ctx, json!({"limit": 2, "cursor": "2"}))
            .await
            .expect("list");
        let text = second.output.expect("output");
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(second.metadata.expect("metadata")["cursor"].is_null());
    }
}
