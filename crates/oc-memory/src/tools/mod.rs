//! The four memory tools exposed to the model.
//!
//! [`MemoryTools::registry`] returns a registry containing only
//! `memory_write`, `memory_search`, `memory_forget` and `memory_list`; the host
//! merges it into its own registry. Scope ids come from the host: the project
//! id is either configured with [`MemoryTools::with_project_id`] or derived
//! from [`ToolContext::directory`] the same way sessions derive it, and the
//! agent id (needed for `scope: "agent"`) must be configured with
//! [`MemoryTools::with_agent_id`]. Session scope uses the calling session.

mod common;
mod forget;
mod list;
mod search;
#[cfg(test)]
mod tests;
mod write;

use std::sync::Arc;

use oc_tools::{ToolContext, ToolError, ToolRegistry};
use oc_types::project_id_from_directory;

use crate::error::MemoryError;
use crate::service::MemoryService;

pub use forget::MemoryForgetTool;
pub use list::MemoryListTool;
pub use search::MemorySearchTool;
pub use write::MemoryWriteTool;

const SCOPES: [&str; 4] = ["global", "project", "agent", "session"];

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
