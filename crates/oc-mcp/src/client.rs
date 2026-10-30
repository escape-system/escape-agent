//! MCP client lifecycle: handshake, catalogs, calls, status and reconnect.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;

use crate::config::{
    LEGACY_PROTOCOL_VERSION, McpServerConfig, Protocol, TimeoutConfig, expand_placeholders,
};
use crate::error::{McpError, McpResult, RpcError};
use crate::protocol::codes;
use crate::tool::McpTool;
use crate::transport::http::{HttpFactory, HttpOptions, HttpTransport};
use crate::transport::stdio::{StdioFactory, StdioOptions, StdioTransport};
use crate::transport::{Transport, TransportEventHandler, TransportFactory};

/// Maximum catalog pages followed before declaring a cursor loop.
const MAX_PAGES: usize = 1_000;

/// Connection status, tagged by `status` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpStatus {
    /// Handshake and initial catalog completed.
    Connected,
    /// Connection is in progress.
    Pending,
    /// Configured but disabled.
    Disabled,
    /// Connection failed.
    Failed { error: String },
    /// The server rejected anonymous access.
    NeedsAuth { error: String },
}

impl McpStatus {
    /// Status name, matching the wire tag.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Pending => "pending",
            Self::Disabled => "disabled",
            Self::Failed { .. } => "failed",
            Self::NeedsAuth { .. } => "needs_auth",
        }
    }
}

/// `mcp.status.changed` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStatusEvent {
    pub server: String,
    pub status: McpStatus,
}

/// A catalog section that changed on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpChange {
    Tools,
    Prompts,
    Resources,
}

impl McpChange {
    /// Wire notification name.
    #[must_use]
    pub fn notification(self) -> &'static str {
        match self {
            Self::Tools => "notifications/tools/list_changed",
            Self::Prompts => "notifications/prompts/list_changed",
            Self::Resources => "notifications/resources/list_changed",
        }
    }
}

/// Server identity from `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub title: Option<String>,
}

/// Client identity sent in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: "opencode".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

impl ClientInfo {
    /// Default client identity.
    #[must_use]
    pub fn opencode() -> Self {
        Self::default()
    }
}

/// Handles server-initiated elicitation requests.
#[async_trait]
pub trait ElicitationHandler: Send + Sync + std::fmt::Debug {
    /// Presents `params` to the user and returns the elicitation result object.
    async fn create(&self, server: &str, params: Value) -> McpResult<Value>;
}

/// Options for one client connection.
#[derive(Clone)]
pub struct ClientOptions {
    /// Location directory exposed through `roots/list`.
    pub directory: PathBuf,
    /// Client identity.
    pub client_info: ClientInfo,
    /// Protocol negotiation mode.
    pub protocol: Protocol,
    /// Effective timeouts.
    pub timeouts: TimeoutConfig,
    /// Elicitation handler; its capabilities are advertised only when present.
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
    /// Status handler installed before the first `pending` event.
    pub status_handler: Option<Arc<dyn Fn(McpStatusEvent) + Send + Sync>>,
    /// Catalog change handler installed before connecting.
    pub change_handler: Option<Arc<dyn Fn(McpChange) + Send + Sync>>,
}

impl std::fmt::Debug for ClientOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientOptions")
            .field("directory", &self.directory)
            .field("client_info", &self.client_info)
            .field("protocol", &self.protocol)
            .field("timeouts", &self.timeouts)
            .field("elicitation", &self.elicitation.is_some())
            .field("status_handler", &self.status_handler.is_some())
            .field("change_handler", &self.change_handler.is_some())
            .finish()
    }
}

impl ClientOptions {
    /// Defaults for a location directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            client_info: ClientInfo::default(),
            protocol: Protocol::Legacy,
            timeouts: TimeoutConfig::default(),
            elicitation: None,
            status_handler: None,
            change_handler: None,
        }
    }
}

type StatusHandler = Arc<dyn Fn(McpStatusEvent) + Send + Sync>;
type ChangeHandler = Arc<dyn Fn(McpChange) + Send + Sync>;

/// `GET /api/mcp` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub status: McpStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "integrationID", skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    status: Option<McpStatus>,
    server_info: Option<ServerInfo>,
    instructions: Option<String>,
    protocol_version: Option<String>,
    capabilities: Value,
    tools: Vec<McpTool>,
    prompts: Vec<Value>,
    resources: Vec<Value>,
    templates: Vec<Value>,
}

impl State {
    fn clear_catalog(&mut self) {
        self.tools.clear();
        self.prompts.clear();
        self.resources.clear();
        self.templates.clear();
    }
}

struct Inner {
    server: String,
    directory: PathBuf,
    client_info: ClientInfo,
    protocol: Protocol,
    timeouts: TimeoutConfig,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
    transport: RwLock<Arc<dyn Transport>>,
    factory: Option<Arc<dyn TransportFactory>>,
    state: Mutex<State>,
    status_handler: RwLock<Option<StatusHandler>>,
    change_handler: RwLock<Option<ChangeHandler>>,
    reconnect_lock: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    closed: AtomicBool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("server", &self.server)
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Inner {
    fn current_transport(&self) -> Arc<dyn Transport> {
        self.transport
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_status(&self, status: McpStatus) {
        lock(&self.state).status = Some(status.clone());
        let handler = self
            .status_handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(handler) = handler {
            handler(McpStatusEvent {
                server: self.server.clone(),
                status,
            });
        }
    }

    fn emit_change(&self, change: McpChange) {
        let handler = self
            .change_handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(handler) = handler {
            handler(change);
        }
    }
}

struct ClientHandler {
    inner: Weak<Inner>,
    generation: u64,
    directory: PathBuf,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
}

impl std::fmt::Debug for ClientHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientHandler")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl ClientHandler {
    fn new(inner: &Arc<Inner>, generation: u64) -> Self {
        Self {
            inner: Arc::downgrade(inner),
            generation,
            directory: inner.directory.clone(),
            elicitation: inner.elicitation.clone(),
        }
    }

    fn roots(&self) -> Value {
        let uri = Url::from_file_path(&self.directory).map_or_else(
            |()| {
                format!(
                    "file://{}",
                    self.directory.to_string_lossy().replace('\\', "/")
                )
            },
            |url| url.to_string(),
        );
        json!({"roots": [{"uri": uri}]})
    }
}

#[async_trait]
impl TransportEventHandler for ClientHandler {
    async fn on_request(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match method {
            "roots/list" => Ok(self.roots()),
            "ping" => Ok(json!({})),
            "elicitation/create" => match (&self.elicitation, method) {
                (Some(handler), _) => {
                    let server = self
                        .inner
                        .upgrade()
                        .map_or_else(String::new, |inner| inner.server.clone());
                    handler
                        .create(&server, params.unwrap_or(Value::Null))
                        .await
                        .map_err(|error| RpcError {
                            code: codes::INTERNAL_ERROR,
                            message: error.to_string(),
                            data: None,
                        })
                }
                (None, _) => Err(RpcError {
                    code: codes::METHOD_NOT_FOUND,
                    message: "elicitation is not supported".to_owned(),
                    data: None,
                }),
            },
            other => Err(RpcError {
                code: codes::METHOD_NOT_FOUND,
                message: format!("unsupported method: {other}"),
                data: None,
            }),
        }
    }

    async fn on_notification(&self, method: &str, _params: Option<Value>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.generation.load(Ordering::SeqCst) != self.generation {
            return;
        }
        let change = match method {
            "notifications/tools/list_changed" => McpChange::Tools,
            "notifications/prompts/list_changed" => McpChange::Prompts,
            "notifications/resources/list_changed" => McpChange::Resources,
            other => {
                tracing::debug!(server = %inner.server, method = %other, "MCP notification");
                return;
            }
        };
        inner.emit_change(change);
        let client = McpClient {
            inner: inner.clone(),
        };
        tokio::spawn(async move {
            let result = match change {
                McpChange::Tools => client.refresh_tools().await.map(|_| ()),
                McpChange::Prompts => client.refresh_prompts().await.map(|_| ()),
                McpChange::Resources => client.refresh_resources().await.map(|_| ()),
            };
            if let Err(error) = result {
                tracing::warn!(server = %client.inner.server, %error, "background MCP catalog refresh failed");
            }
        });
    }

    async fn on_close(&self, reason: String) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        if inner.generation.load(Ordering::SeqCst) != self.generation
            || inner.closed.load(Ordering::SeqCst)
        {
            return;
        }
        {
            let mut state = lock(&inner.state);
            state.clear_catalog();
        }
        inner.set_status(McpStatus::Failed {
            error: reason.clone(),
        });
        tracing::warn!(server = %inner.server, %reason, "MCP connection closed");
    }

    fn on_error(&self, message: String) {
        if let Some(inner) = self.inner.upgrade() {
            tracing::warn!(server = %inner.server, %message, "MCP transport error");
        }
    }
}

#[derive(Debug)]
struct InitResult {
    protocol_version: String,
    capabilities: Value,
    server_info: Option<ServerInfo>,
    instructions: Option<String>,
}

/// A connected MCP server client.
#[derive(Debug, Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

impl McpClient {
    /// Connects using a configured server entry.
    ///
    /// Disabled servers are rejected; callers represent them with
    /// [`McpStatus::Disabled`].
    pub async fn connect(
        name: &str,
        config: &McpServerConfig,
        options: ClientOptions,
    ) -> McpResult<Self> {
        if config.disabled() {
            return Err(McpError::NotConnected {
                server: name.to_owned(),
                reason: "disabled".to_owned(),
            });
        }
        let timeouts = options.timeouts.merged_with(&config.timeout());
        let (transport, factory): (Arc<dyn Transport>, Option<Arc<dyn TransportFactory>>) =
            match config {
                McpServerConfig::Local(local) => {
                    let cwd = match &local.cwd {
                        Some(cwd) if Path::new(cwd).is_absolute() => PathBuf::from(cwd),
                        Some(cwd) => options.directory.join(cwd),
                        None => options.directory.clone(),
                    };
                    let environment = expand_placeholders(&local.environment, &host_environment())?;
                    let stdio = StdioOptions {
                        server: name.to_owned(),
                        command: local.command.clone(),
                        cwd,
                        environment,
                    };
                    let transport = StdioTransport::spawn(stdio.clone())?;
                    (transport, Some(Arc::new(StdioFactory::new(stdio))))
                }
                McpServerConfig::Remote(remote) => {
                    let url = Url::parse(&remote.url)
                        .map_err(|error| McpError::Config(format!("invalid MCP URL: {error}")))?;
                    let headers = expand_placeholders(&remote.headers, &host_environment())?;
                    let http = HttpOptions {
                        server: name.to_owned(),
                        url,
                        headers,
                        protocol_version: None,
                    };
                    let transport = HttpTransport::new(http.clone())?;
                    (transport, Some(Arc::new(HttpFactory::new(http))))
                }
            };
        let options = ClientOptions {
            timeouts,
            protocol: config.protocol(),
            ..options
        };
        Self::connect_transport(name, transport, factory, options).await
    }

    /// Connects over an already-open transport (used by tests and attached
    /// transports). `factory`, when present, enables one reconnect on session
    /// expiry.
    pub async fn connect_transport(
        name: &str,
        transport: Arc<dyn Transport>,
        factory: Option<Arc<dyn TransportFactory>>,
        options: ClientOptions,
    ) -> McpResult<Self> {
        let inner = Arc::new(Inner {
            server: name.to_owned(),
            directory: options.directory.clone(),
            client_info: options.client_info.clone(),
            protocol: options.protocol,
            timeouts: options.timeouts.clone(),
            elicitation: options.elicitation.clone(),
            transport: RwLock::new(transport.clone()),
            factory,
            state: Mutex::new(State::default()),
            status_handler: RwLock::new(options.status_handler.clone()),
            change_handler: RwLock::new(options.change_handler.clone()),
            reconnect_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        let client = Self {
            inner: inner.clone(),
        };
        inner.set_status(McpStatus::Pending);
        transport.set_event_handler(Arc::new(ClientHandler::new(&inner, 1)));
        let result = async {
            let init = handshake(&inner, &transport).await?;
            transport.set_protocol_version(&init.protocol_version);
            apply_init(&inner, init);
            transport.notify("notifications/initialized", None).await?;
            client.refresh_tools().await?;
            inner.set_status(McpStatus::Connected);
            Ok(())
        }
        .await;
        match result {
            Ok(()) => Ok(client),
            Err(error) => {
                let status = match &error {
                    McpError::NeedsAuth { message, .. } => McpStatus::NeedsAuth {
                        error: message.clone(),
                    },
                    other => McpStatus::Failed {
                        error: other.to_string(),
                    },
                };
                inner.set_status(status);
                transport.shutdown().await;
                Err(error)
            }
        }
    }

    /// Server name from config.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.inner.server
    }

    /// Child process id for a stdio server.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.inner.current_transport().pid()
    }

    /// Session id minted by an HTTP server.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.inner.current_transport().session_id()
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> McpStatus {
        lock(&self.inner.state)
            .status
            .clone()
            .unwrap_or(McpStatus::Pending)
    }

    /// `GET /api/mcp` entry for this client.
    #[must_use]
    pub fn info(&self) -> McpServerInfo {
        let title = lock(&self.inner.state)
            .server_info
            .as_ref()
            .and_then(|info| info.title.clone());
        McpServerInfo {
            name: self.inner.server.clone(),
            status: self.status(),
            title,
            integration_id: None,
        }
    }

    /// Server identity from `initialize`.
    #[must_use]
    pub fn server_info(&self) -> Option<ServerInfo> {
        lock(&self.inner.state).server_info.clone()
    }

    /// Server instructions.
    #[must_use]
    pub fn instructions(&self) -> Option<String> {
        lock(&self.inner.state).instructions.clone()
    }

    /// Negotiated protocol revision.
    #[must_use]
    pub fn protocol_version(&self) -> Option<String> {
        lock(&self.inner.state).protocol_version.clone()
    }

    /// Advertised capabilities.
    #[must_use]
    pub fn capabilities(&self) -> Value {
        lock(&self.inner.state).capabilities.clone()
    }

    /// Installs a status change handler.
    pub fn set_status_handler(&self, handler: Arc<dyn Fn(McpStatusEvent) + Send + Sync>) {
        *self
            .inner
            .status_handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    /// Installs a catalog change handler.
    pub fn set_change_handler(&self, handler: Arc<dyn Fn(McpChange) + Send + Sync>) {
        *self
            .inner
            .change_handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    fn supports(&self, capability: &str) -> bool {
        lock(&self.inner.state)
            .capabilities
            .get(capability)
            .is_some_and(|value| !value.is_null())
    }

    /// Sends one request, reconnecting once when the HTTP session expired.
    async fn request_recovering(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> McpResult<Value> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(McpError::NotConnected {
                server: self.inner.server.clone(),
                reason: "client is shut down".to_owned(),
            });
        }
        let generation = self.inner.generation.load(Ordering::SeqCst);
        let transport = self.inner.current_transport();
        match transport.request(method, params.clone(), timeout).await {
            Err(McpError::SessionExpired) => {
                tracing::info!(server = %self.inner.server, "MCP session expired, reconnecting");
                self.reconnect(generation).await?;
                self.inner
                    .current_transport()
                    .request(method, params, timeout)
                    .await
            }
            other => other,
        }
    }

    async fn reconnect(&self, observed_generation: u64) -> McpResult<()> {
        let _guard = self.inner.reconnect_lock.lock().await;
        if self.inner.generation.load(Ordering::SeqCst) != observed_generation {
            return Ok(());
        }
        let Some(factory) = self.inner.factory.clone() else {
            self.inner.set_status(McpStatus::Failed {
                error: "MCP session expired and the transport cannot reconnect".to_owned(),
            });
            return Err(McpError::NotConnected {
                server: self.inner.server.clone(),
                reason: "session expired".to_owned(),
            });
        };
        let previous = self.inner.current_transport();
        let transport = factory.create().await?;
        let generation = observed_generation + 1;
        transport.set_event_handler(Arc::new(ClientHandler::new(&self.inner, generation)));
        let init = match handshake(&self.inner, &transport).await {
            Ok(init) => init,
            Err(error) => {
                transport.shutdown().await;
                self.inner.set_status(McpStatus::Failed {
                    error: error.to_string(),
                });
                return Err(error);
            }
        };
        transport.set_protocol_version(&init.protocol_version);
        apply_init(&self.inner, init);
        if let Err(error) = transport.notify("notifications/initialized", None).await {
            transport.shutdown().await;
            self.inner.set_status(McpStatus::Failed {
                error: error.to_string(),
            });
            return Err(error);
        }
        *self
            .inner
            .transport
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = transport;
        self.inner.generation.store(generation, Ordering::SeqCst);
        tokio::spawn(async move {
            previous.shutdown().await;
        });
        self.inner.set_status(McpStatus::Connected);
        Ok(())
    }

    async fn list_pages(&self, method: &str, key: &str) -> McpResult<Vec<Value>> {
        let timeout = self.inner.timeouts.catalog();
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self
                .request_recovering(method, Some(params), timeout)
                .await?;
            let page = result
                .get(key)
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol(format!("{method} result has no {key} array")))?;
            items.extend(page.iter().cloned());
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            if let Some(next) = &cursor
                && !seen.insert(next.clone())
            {
                return Err(McpError::Protocol(format!(
                    "{method} pagination repeated a cursor"
                )));
            }
            if cursor.is_none() {
                return Ok(items);
            }
        }
        Err(McpError::Protocol(format!(
            "{method} exceeded {MAX_PAGES} pages"
        )))
    }

    /// Refreshes and returns the tool catalog, dropping invalid tools with a
    /// warning.
    pub async fn refresh_tools(&self) -> McpResult<Vec<McpTool>> {
        let raw = if self.supports("tools") {
            self.list_pages("tools/list", "tools").await?
        } else {
            Vec::new()
        };
        let mut tools = Vec::new();
        for value in raw {
            match McpTool::from_wire(&self.inner.server, &value) {
                Ok(tool) => tools.push(tool),
                Err(error) => {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>");
                    tracing::warn!(
                        server = %self.inner.server,
                        tool = %name,
                        %error,
                        "skipping invalid MCP tool"
                    );
                }
            }
        }
        {
            let mut state = lock(&self.inner.state);
            state.tools.clone_from(&tools);
        }
        Ok(tools)
    }

    /// Alias for [`Self::refresh_tools`].
    pub async fn list_tools(&self) -> McpResult<Vec<McpTool>> {
        self.refresh_tools().await
    }

    /// Cached tools (no request).
    #[must_use]
    pub fn tools(&self) -> Vec<McpTool> {
        lock(&self.inner.state).tools.clone()
    }

    /// Calls a tool with the server's own tool name.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> McpResult<Value> {
        let timeout = self.inner.timeouts.execution();
        let params = json!({"name": name, "arguments": arguments});
        self.request_recovering("tools/call", Some(params), timeout)
            .await
            .map_err(|error| match error {
                McpError::ToolReturnedError { .. } => error,
                McpError::Timeout(after) => McpError::ToolCall {
                    server: self.inner.server.clone(),
                    tool: name.to_owned(),
                    message: format!("MCP tool timed out after {}s", after.as_secs()),
                },
                other => McpError::ToolCall {
                    server: self.inner.server.clone(),
                    tool: name.to_owned(),
                    message: other.to_string(),
                },
            })
    }

    /// Refreshes and returns the prompt catalog.
    pub async fn refresh_prompts(&self) -> McpResult<Vec<Value>> {
        let prompts = if self.supports("prompts") {
            self.list_pages("prompts/list", "prompts").await?
        } else {
            Vec::new()
        };
        {
            let mut state = lock(&self.inner.state);
            state.prompts.clone_from(&prompts);
        }
        Ok(prompts)
    }

    /// Cached prompts.
    #[must_use]
    pub fn prompts(&self) -> Vec<Value> {
        lock(&self.inner.state).prompts.clone()
    }

    /// Gets one prompt.
    pub async fn get_prompt(&self, name: &str, arguments: Option<Value>) -> McpResult<Value> {
        let timeout = self.inner.timeouts.execution();
        let params = json!({"name": name, "arguments": arguments.unwrap_or_else(|| json!({}))});
        self.request_recovering("prompts/get", Some(params), timeout)
            .await
    }

    /// Refreshes resource and resource-template catalogs.
    pub async fn refresh_resources(&self) -> McpResult<Vec<Value>> {
        if !self.supports("resources") {
            let mut state = lock(&self.inner.state);
            state.resources.clear();
            state.templates.clear();
            return Ok(Vec::new());
        }
        let resources = self.list_pages("resources/list", "resources").await?;
        // Some servers declare resources without implementing template listing.
        let templates = match self
            .list_pages("resources/templates/list", "resourceTemplates")
            .await
        {
            Ok(templates) => templates,
            Err(error) => {
                tracing::debug!(server = %self.inner.server, %error, "MCP resource templates unavailable");
                Vec::new()
            }
        };
        let mut state = lock(&self.inner.state);
        state.resources.clone_from(&resources);
        state.templates = templates;
        drop(state);
        Ok(resources)
    }

    /// Cached resources.
    #[must_use]
    pub fn resources(&self) -> Vec<Value> {
        lock(&self.inner.state).resources.clone()
    }

    /// Cached resource templates.
    #[must_use]
    pub fn resource_templates(&self) -> Vec<Value> {
        lock(&self.inner.state).templates.clone()
    }

    /// Reads one resource; `None` when the server has no resources capability.
    pub async fn read_resource(&self, uri: &str) -> McpResult<Option<Value>> {
        if !self.supports("resources") {
            return Ok(None);
        }
        let timeout = self.inner.timeouts.execution();
        self.request_recovering("resources/read", Some(json!({"uri": uri})), timeout)
            .await
            .map(Some)
    }

    /// Pings the server.
    pub async fn ping(&self) -> McpResult<Value> {
        self.request_recovering("ping", None, self.inner.timeouts.catalog())
            .await
    }

    /// Closes the transport and marks the client disabled.
    pub async fn shutdown(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.current_transport().shutdown().await;
        lock(&self.inner.state).clear_catalog();
        self.inner.set_status(McpStatus::Disabled);
    }
}

fn host_environment() -> std::collections::HashMap<String, String> {
    std::env::vars().collect()
}

fn apply_init(inner: &Arc<Inner>, init: InitResult) {
    let mut state = lock(&inner.state);
    state.protocol_version = Some(init.protocol_version);
    state.capabilities = init.capabilities;
    state.server_info = init.server_info;
    state.instructions = init.instructions;
}

fn capabilities(inner: &Inner) -> Value {
    let mut capabilities = Map::new();
    capabilities.insert("roots".to_owned(), json!({}));
    capabilities.insert("tools".to_owned(), json!({"listChanged": true}));
    capabilities.insert("prompts".to_owned(), json!({"listChanged": true}));
    capabilities.insert("resources".to_owned(), json!({"listChanged": true}));
    if inner.elicitation.is_some() {
        capabilities.insert("elicitation".to_owned(), json!({"form": {}, "url": {}}));
    }
    Value::Object(capabilities)
}

fn is_protocol_rejection(error: &McpError) -> bool {
    matches!(
        error,
        McpError::Rpc(_) | McpError::Protocol(_) | McpError::Closed | McpError::Exited(_)
    ) || matches!(error, McpError::Http { status, .. } if *status == 400 || *status == 404)
}

fn unsupported_message(error: &McpError) -> String {
    let supported = match error {
        McpError::Rpc(rpc) => rpc
            .data
            .as_ref()
            .and_then(|data| data.get("supported"))
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            }),
        _ => None,
    };
    match supported {
        Some(supported) if !supported.is_empty() => {
            format!("{error}; the server supports {supported}")
        }
        _ => error.to_string(),
    }
}

async fn initialize_once(
    inner: &Arc<Inner>,
    transport: &Arc<dyn Transport>,
    version: &str,
) -> McpResult<InitResult> {
    let params = json!({
        "protocolVersion": version,
        "capabilities": capabilities(inner),
        "clientInfo": {
            "name": inner.client_info.name,
            "version": inner.client_info.version,
        }
    });
    let result = transport
        .request("initialize", Some(params), inner.timeouts.startup())
        .await?;
    let Some(object) = result.as_object() else {
        return Err(McpError::Protocol(
            "initialize result is not an object".to_owned(),
        ));
    };
    let protocol_version = object
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol("initialize result has no protocolVersion".to_owned()))?
        .to_owned();
    let server_info = object.get("serverInfo").and_then(|value| {
        let name = value.get("name")?.as_str()?.to_owned();
        let version = value.get("version")?.as_str()?.to_owned();
        let title = value
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        Some(ServerInfo {
            name,
            version,
            title,
        })
    });
    let instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    Ok(InitResult {
        protocol_version,
        capabilities: object
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({})),
        server_info,
        instructions,
    })
}

async fn handshake(inner: &Arc<Inner>, transport: &Arc<dyn Transport>) -> McpResult<InitResult> {
    let initial = inner.protocol.initial_version();
    match initialize_once(inner, transport, initial).await {
        Ok(init) => {
            if inner.protocol == Protocol::V2026_07_28 && init.protocol_version != initial {
                return Err(McpError::Protocol(format!(
                    "MCP server negotiated {} but protocol 2026-07-28 was required",
                    init.protocol_version
                )));
            }
            Ok(init)
        }
        Err(error)
            if inner.protocol.allows_legacy_fallback()
                && initial != LEGACY_PROTOCOL_VERSION
                && is_protocol_rejection(&error) =>
        {
            tracing::info!(
                server = %inner.server,
                error = %error,
                "MCP 2026-07-28 probe rejected; falling back to the legacy initialize handshake"
            );
            initialize_once(inner, transport, LEGACY_PROTOCOL_VERSION)
                .await
                .map_err(|fallback| {
                    McpError::Protocol(format!(
                        "{}; legacy fallback failed: {}",
                        unsupported_message(&error),
                        fallback
                    ))
                })
        }
        Err(error) => Err(error),
    }
}
