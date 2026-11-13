use std::sync::Arc;

use futures_util::future::BoxFuture;
use oc_config::ConfigService;
use oc_llm::{Catalog, LlmError, LlmRequest, LlmStream, SharedLlmClient};
use oc_store::Store;
use oc_tools::ToolRegistry;
use oc_types::{EventEnvelope, SessionInfo, StructuredError};

/// Sink that publishes protocol events. The server implements this over its SSE
/// bus; the runtime never depends on `oc-server`.
pub trait EventSink: Send + Sync {
    /// Publish one envelope. Must not block; the runtime calls it from the
    /// session actor.
    fn publish(&self, event: EventEnvelope);
}

/// A permission question raised while a tool is being authorised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAsk {
    pub session_id: String,
    pub message_id: String,
    pub tool_call_id: String,
    pub tool: String,
    pub action: String,
    pub resources: Vec<String>,
    /// Human-readable prompt for the client; optional.
    pub message: Option<String>,
}

/// Result of asking the host to resolve a permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { message: String },
}

/// Host-provided permission resolver. `Ask` rules are handed to this handler;
/// `Allow`/`Deny` config rules never reach it.
pub trait PermissionHandler: Send + Sync {
    fn resolve(&self, request: PermissionAsk) -> BoxFuture<'static, PermissionDecision>;
}

/// Provider call surface used by the turn loop. The production implementation
/// is [`LlmClientAdapter`]; tests supply scripted implementations so no HTTP
/// request is ever made.
pub trait ModelClient: Send + Sync {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>>;
}

/// [`ModelClient`] over the real [`SharedLlmClient`].
#[derive(Debug, Clone)]
pub struct LlmClientAdapter {
    client: Arc<SharedLlmClient>,
}

impl LlmClientAdapter {
    #[must_use]
    pub fn new(client: Arc<SharedLlmClient>) -> Self {
        Self { client }
    }
}

impl ModelClient for LlmClientAdapter {
    fn stream(&self, request: LlmRequest) -> BoxFuture<'static, Result<LlmStream, LlmError>> {
        let client = Arc::clone(&self.client);
        Box::pin(async move { client.0.stream(request).await })
    }
}

/// Host-supplied tool source. The runtime calls every provider once per
/// execution and merges the returned tools after the static registry, so a
/// provider may replace a built-in by name.
///
/// `session` is the resolved session row and `agent` the agent id that owns
/// the turn. Providers must be cheap and synchronous: the turn loop holds no
/// async lock across the call.
pub trait ToolProvider: Send + Sync {
    /// Tools visible to this session/agent combination.
    fn tools(&self, session: &SessionInfo, agent: &str) -> Vec<Arc<dyn oc_tools::Tool>>;
}

/// Host-supplied post-edit diagnostics (typically LSP errors/warnings) so
/// `oc-runtime` stays decoupled from `oc-lsp`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostEditDiagnostics {
    /// Preformatted diagnostic block (for example `<lsp_diagnostics>...</lsp_diagnostics>`),
    /// omitted when there are no actionable diagnostics.
    pub formatted: Option<String>,
    /// Total diagnostic count surfaced for the edited files.
    pub error_count: usize,
    /// Optional structured diagnostic payload stored on `metadata.diagnostics`.
    pub metadata: Option<serde_json::Value>,
}

/// Optional host hook invoked after non-preview code mutations.
#[async_trait::async_trait]
pub trait EditDiagnosticsHook: Send + Sync {
    async fn after_edit(&self, root: &std::path::Path, files: &[String]) -> PostEditDiagnostics;
}

/// Construction inputs for [`crate::Runtime`].
#[derive(Clone)]
pub struct RuntimeConfig {
    pub store: Arc<Store>,
    pub config: Arc<ConfigService>,
    pub catalog: Arc<Catalog>,
    pub client: Arc<SharedLlmClient>,
    pub tools: Arc<ToolRegistry>,
    pub sink: Arc<dyn EventSink>,
    pub permissions: Arc<dyn PermissionHandler>,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("store", &self.store)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

/// Failures the runtime reports to its host. Turn-level model failures are not
/// returned from [`crate::Runtime::notify`]; they are persisted and published as
/// `session.execution.failed` so the actor can continue with the next prompt.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] oc_store::StoreError),
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("invalid session: {0}")]
    InvalidSession(String),
    #[error("execution cancelled")]
    Cancelled,
    #[error("runtime is shut down")]
    Shutdown,
    #[error("timed out waiting for session {0} to become idle")]
    Timeout(String),
    #[error("task failed: {0}")]
    Task(String),
}

impl RuntimeError {
    /// Wire error used by assistant messages and terminal execution events.
    #[must_use]
    pub fn structured(&self) -> StructuredError {
        let kind = match self {
            Self::Store(_) | Self::Task(_) => "InternalError",
            Self::Llm(_) => "ProviderError",
            Self::Config(_) => "ConfigError",
            Self::SessionNotFound(_) | Self::InvalidSession(_) => "NotFoundError",
            Self::AgentNotFound(_) => "AgentNotFoundError",
            Self::ModelUnavailable(_) => "ModelNotFoundError",
            Self::Cancelled | Self::Shutdown => "MessageAbortedError",
            Self::Timeout(_) => "TimeoutError",
        };
        StructuredError::new(kind, self.to_string())
    }
}
