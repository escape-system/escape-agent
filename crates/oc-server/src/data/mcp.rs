//! `/api/mcp` status, catalogs and manual lifecycle routes.
//!
//! The manager owns the configured server map and one client slot per server.
//! Nothing connects at startup: a server stays `disconnected` (or `disabled`
//! when configuration turns it off) until an explicit
//! `POST /api/experimental/mcp/:server/connect`. Failed connects are recorded
//! and reported truthfully; connects to the same server are serialized so two
//! requests cannot spawn two processes.
//!
//! `PUT`/`DELETE /api/experimental/mcp/:server` merge `mcp.servers.<name>` into
//! the highest-precedence global `opencode.jsonc` (atomic rewrite), update the
//! in-memory map and, for deletion, shut the client down. Connected tool
//! snapshots are cached so [`McpManager::connected_tools`] is synchronous for
//! the runtime's tool provider.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put as put_route};
use oc_config::ConfigService;
use oc_mcp::{ClientOptions, McpClient, McpConfig, McpError, McpServerConfig, McpStatus, McpTool};
use oc_types::{ApiError, EventEnvelope, LocationEnvelope};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::AppState;
use crate::data::config_patch;
use crate::data::location::{self, RequestLocation};
use crate::data::query_error;
use crate::data::session::LocationQuery;
use crate::error::ErrorResponse;

/// Event published whenever a server's connection status changes.
pub const STATUS_EVENT: &str = "mcp.status.changed";

/// Connected catalog sizes returned by `POST …/connect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectCounts {
    /// Number of tools advertised by the server.
    pub tools: usize,
    /// Number of prompts advertised by the server.
    pub prompts: usize,
    /// Number of resources advertised by the server.
    pub resources: usize,
}

/// One server's connection slot.
#[derive(Clone)]
enum ClientState {
    Disconnected,
    Connected(Arc<McpClient>),
    Failed { error: String },
    NeedsAuth,
}

/// Configured MCP servers plus their lazily created clients.
pub struct McpManager {
    servers: RwLock<BTreeMap<String, McpServerConfig>>,
    directory: PathBuf,
    /// Highest-precedence global config directory used by `PUT`/`DELETE`.
    config_dir: PathBuf,
    clients: Mutex<HashMap<String, ClientState>>,
    connect_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    tool_cache: RwLock<Vec<(Arc<McpClient>, McpTool)>>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = read_lock(&self.servers).keys().cloned().collect();
        f.debug_struct("McpManager")
            .field("servers", &names)
            .field("directory", &self.directory)
            .field("config_dir", &self.config_dir)
            .finish_non_exhaustive()
    }
}

impl McpManager {
    /// Build a manager from an already parsed MCP document.
    ///
    /// Persistence goes to the process-resolved global config directory; use
    /// [`Self::with_config_dir`] to override it (tests, non-standard hosts).
    #[must_use]
    pub fn new(config: McpConfig, directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        let connect_locks = config
            .servers
            .keys()
            .map(|name| (name.clone(), Arc::new(Mutex::new(()))))
            .collect();
        Self {
            servers: RwLock::new(config.servers),
            directory,
            config_dir: config_patch::global_dir(),
            clients: Mutex::new(HashMap::new()),
            connect_locks: RwLock::new(connect_locks),
            tool_cache: RwLock::new(Vec::new()),
        }
    }

    /// Parse `mcp` from a merged configuration document.
    ///
    /// CLI wiring:
    /// `McpManager::from_config(services.config.as_ref(), &directory)?`.
    pub fn from_config(
        config: &ConfigService,
        directory: impl Into<PathBuf>,
    ) -> Result<Self, McpError> {
        let document = config.merged().get("mcp").cloned().unwrap_or(Value::Null);
        let mut manager = Self::new(McpConfig::from_value(&document)?, directory);
        if let Some(dir) = config.global_config_dir() {
            manager.config_dir = dir;
        }
        Ok(manager)
    }

    /// Override the directory receiving `mcp.servers.<name>` writes.
    #[must_use]
    pub fn with_config_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.config_dir = dir.into();
        self
    }

    /// Configured servers, keyed by name.
    #[must_use]
    pub fn servers(&self) -> BTreeMap<String, McpServerConfig> {
        read_lock(&self.servers).clone()
    }

    /// One configured server.
    #[must_use]
    pub fn server(&self, name: &str) -> Option<McpServerConfig> {
        read_lock(&self.servers).get(name).cloned()
    }

    fn connect_lock(&self, name: &str) -> Arc<Mutex<()>> {
        if let Some(lock) = read_lock(&self.connect_locks).get(name) {
            return Arc::clone(lock);
        }
        let mut locks = write_lock(&self.connect_locks);
        Arc::clone(
            locks
                .entry(name.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Connect, initialize and fetch all catalogs for `name`.
    ///
    /// A healthy existing client is reused, so repeated connects cannot spawn
    /// duplicate child processes.
    pub async fn connect(&self, name: &str) -> Result<ConnectCounts, McpError> {
        let config = read_lock(&self.servers)
            .get(name)
            .cloned()
            .ok_or_else(|| McpError::NotFound(name.to_owned()))?;
        if config.disabled() {
            return Err(McpError::NotConnected {
                server: name.to_owned(),
                reason: "disabled".to_owned(),
            });
        }
        let lock = self.connect_lock(name);
        let _guard = lock.lock().await;

        let existing = self.clients.lock().await.get(name).cloned();
        if let Some(ClientState::Connected(client)) = existing {
            if client.status() == McpStatus::Connected {
                self.rebuild_tool_cache().await;
                return Ok(counts(&client));
            }
            client.shutdown().await;
        }
        self.clients
            .lock()
            .await
            .insert(name.to_owned(), ClientState::Disconnected);

        let options = ClientOptions::new(self.directory.clone());
        let client = match McpClient::connect(name, &config, options).await {
            Ok(client) => Arc::new(client),
            Err(error) => {
                let state = match &error {
                    McpError::NeedsAuth { .. } => ClientState::NeedsAuth,
                    _ => ClientState::Failed {
                        error: error.to_string(),
                    },
                };
                self.clients.lock().await.insert(name.to_owned(), state);
                return Err(error);
            }
        };

        // `McpClient::connect` already refreshed `tools/list`; fetch the other
        // two catalogs so status counts and the resource route are populated.
        let mut refreshed = client.refresh_prompts().await.map(|_| ());
        if refreshed.is_ok() {
            refreshed = client.refresh_resources().await.map(|_| ());
        }
        if let Err(error) = refreshed {
            client.shutdown().await;
            self.clients.lock().await.insert(
                name.to_owned(),
                ClientState::Failed {
                    error: error.to_string(),
                },
            );
            return Err(error);
        }

        let counts = counts(&client);
        self.clients
            .lock()
            .await
            .insert(name.to_owned(), ClientState::Connected(Arc::clone(&client)));
        self.rebuild_tool_cache().await;
        Ok(counts)
    }

    /// Shut a connected client down and mark the server disconnected.
    pub async fn disconnect(&self, name: &str) {
        let lock = self.connect_lock(name);
        let _guard = lock.lock().await;
        let previous = self.clients.lock().await.remove(name);
        if let Some(ClientState::Connected(client)) = previous {
            client.shutdown().await;
        }
        self.clients
            .lock()
            .await
            .insert(name.to_owned(), ClientState::Disconnected);
        self.rebuild_tool_cache().await;
    }

    /// Add or replace a server, persisting `mcp.servers.<name>`.
    ///
    /// A replaced server's client is shut down; the new configuration takes
    /// effect on the next explicit connect.
    pub async fn put(&self, name: &str, config: Value) -> Result<(), McpError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(McpError::Config(
                "MCP server name must not be empty".to_owned(),
            ));
        }
        let parsed = McpServerConfig::from_value(&format!("mcp.servers.{name}"), &config)?;
        persist_server(&self.config_dir, name, Some(&config))
            .map_err(|error| config_error(&error))?;
        let previous = write_lock(&self.servers).insert(name.to_owned(), parsed);
        self.connect_lock(name);
        if previous.is_some() {
            self.disconnect(name).await;
        }
        Ok(())
    }

    /// Remove a server, persist the removal and shut its client down.
    ///
    /// Returns `false` when no such server is configured.
    pub async fn delete(&self, name: &str) -> Result<bool, McpError> {
        if !read_lock(&self.servers).contains_key(name) {
            return Ok(false);
        }
        persist_server(&self.config_dir, name, None).map_err(|error| config_error(&error))?;
        let _ = write_lock(&self.servers).remove(name);
        self.disconnect(name).await;
        let _ = write_lock(&self.connect_locks).remove(name);
        Ok(true)
    }

    /// Refresh the cached tool snapshot from connected clients.
    ///
    /// Call after any lifecycle or catalog refresh; connectors call it
    /// themselves. Errors are ignored: a stale catalog entry is dropped rather
    /// than keeping a tool from a client that no longer answers.
    pub async fn refresh_tool_cache(&self) {
        self.rebuild_tool_cache().await;
    }

    async fn rebuild_tool_cache(&self) {
        let order: Vec<String> = read_lock(&self.servers).keys().cloned().collect();
        let clients = self.clients.lock().await;
        let mut cache = Vec::new();
        for name in order {
            let Some(ClientState::Connected(client)) = clients.get(&name) else {
                continue;
            };
            if client.status() != McpStatus::Connected {
                continue;
            }
            for tool in client.tools() {
                cache.push((Arc::clone(client), tool));
            }
        }
        drop(clients);
        *write_lock(&self.tool_cache) = cache;
    }

    /// Snapshot of every tool on every connected server, in configuration
    /// order. Synchronous: the runtime calls this per execution.
    #[must_use]
    pub fn connected_tools(&self) -> Vec<(Arc<McpClient>, McpTool)> {
        read_lock(&self.tool_cache).clone()
    }

    /// Status rows for `GET /api/mcp`, in configuration order.
    pub async fn snapshot(&self) -> Vec<Value> {
        let servers = self.servers();
        let clients = self.clients.lock().await;
        servers
            .iter()
            .map(|(name, config)| {
                let state = clients
                    .get(name)
                    .cloned()
                    .unwrap_or(ClientState::Disconnected);
                let mut row = Map::new();
                row.insert("name".to_owned(), json!(name));
                row.insert(
                    "type".to_owned(),
                    json!(match config {
                        McpServerConfig::Local(_) => "local",
                        McpServerConfig::Remote(_) => "remote",
                    }),
                );
                if config.disabled() {
                    row.insert("status".to_owned(), json!("disabled"));
                    return Value::Object(row);
                }
                let (status, error, counts) = describe(&state);
                row.insert("status".to_owned(), json!(status));
                if let Some(error) = error {
                    row.insert("error".to_owned(), json!(error));
                }
                if let Some(counts) = counts {
                    row.insert("tools".to_owned(), json!(counts.tools));
                    row.insert("prompts".to_owned(), json!(counts.prompts));
                    row.insert("resources".to_owned(), json!(counts.resources));
                }
                Value::Object(row)
            })
            .collect()
    }

    /// Resource/template catalog for `GET /api/mcp/resource`.
    ///
    /// Every connected server's cached catalogs are merged into one
    /// `{resources, templates}` object; each row is tagged with its `server`.
    /// Disconnected and disabled servers contribute nothing.
    pub async fn resource_catalog(&self) -> Value {
        let servers = self.servers();
        let clients = self.clients.lock().await;
        let mut resources = Vec::new();
        let mut templates = Vec::new();
        for name in servers.keys() {
            let Some(ClientState::Connected(client)) = clients.get(name) else {
                continue;
            };
            if client.status() != McpStatus::Connected {
                continue;
            }
            for resource in client.resources() {
                resources.push(tag_server(name, &resource));
            }
            for template in client.resource_templates() {
                templates.push(tag_server(name, &template));
            }
        }
        json!({ "resources": resources, "templates": templates })
    }
}

fn tag_server(server: &str, item: &Value) -> Value {
    let mut object = item.as_object().cloned().unwrap_or_default();
    object.insert("server".to_owned(), json!(server));
    Value::Object(object)
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn config_error(error: &ApiError) -> McpError {
    if error.status() == 400 {
        McpError::Config(error.to_string())
    } else {
        McpError::Io(io::Error::other(error.to_string()))
    }
}

/// Merge (or remove, for `None`) one `mcp.servers.<name>` entry.
fn persist_server(dir: &Path, name: &str, config: Option<&Value>) -> Result<(), ApiError> {
    config_patch::update_document(dir, |document| {
        let mcp = document
            .entry("mcp".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !mcp.is_object() {
            return Err(ApiError::bad_request(
                "existing config mcp is not an object",
            ));
        }
        let servers = mcp
            .as_object_mut()
            .expect("checked above")
            .entry("servers".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !servers.is_object() {
            return Err(ApiError::bad_request(
                "existing config mcp.servers is not an object",
            ));
        }
        let empty = {
            let servers = servers.as_object_mut().expect("checked above");
            match config {
                Some(config) => {
                    servers.insert(name.to_owned(), config.clone());
                }
                None => {
                    let _ = servers.remove(name);
                }
            }
            servers.is_empty()
        };
        if empty {
            let remove_mcp = match document.get_mut("mcp").and_then(Value::as_object_mut) {
                Some(mcp) => {
                    let _ = mcp.remove("servers");
                    mcp.is_empty()
                }
                None => false,
            };
            if remove_mcp {
                let _ = document.remove("mcp");
            }
        }
        Ok(())
    })
    .map(|_| ())
}

fn counts(client: &McpClient) -> ConnectCounts {
    ConnectCounts {
        tools: client.tools().len(),
        prompts: client.prompts().len(),
        resources: client.resources().len(),
    }
}

fn describe(state: &ClientState) -> (&'static str, Option<String>, Option<ConnectCounts>) {
    match state {
        ClientState::Disconnected => ("disconnected", None, None),
        ClientState::Failed { error } => ("failed", Some(error.clone()), None),
        ClientState::NeedsAuth => ("needs_auth", None, None),
        ClientState::Connected(client) => match client.status() {
            McpStatus::Connected => ("connected", None, Some(counts(client))),
            McpStatus::Pending | McpStatus::Disabled => ("disconnected", None, None),
            McpStatus::Failed { error } => ("failed", Some(error), None),
            McpStatus::NeedsAuth { error } => ("needs_auth", Some(error), None),
        },
    }
}

fn require_manager(state: &AppState) -> Result<&Arc<McpManager>, ApiError> {
    state
        .mcp()
        .ok_or_else(|| ApiError::service_unavailable("MCP manager not attached"))
}

fn envelope(location: &RequestLocation, data: Value) -> Json<LocationEnvelope<Value>> {
    Json(LocationEnvelope::new(location.reference(), data))
}

fn server_not_found(name: &str) -> ApiError {
    ApiError::not_found(
        "McpServerNotFoundError",
        format!("MCP server not found: {name}"),
    )
    .with("server", json!(name))
}

fn mcp_error(error: McpError) -> ErrorResponse {
    match error {
        McpError::Config(message) => ApiError::bad_request(message).into(),
        McpError::NotFound(name) => server_not_found(&name).into(),
        error => ApiError::internal(format!("MCP config write failed: {error}")).into(),
    }
}

fn publish_status(state: &AppState, server: &str, status: &str) {
    state.publish(EventEnvelope::now(
        STATUS_EVENT,
        json!({"server": server, "status": status}),
    ));
}

/// `GET /api/mcp`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_manager(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let rows = manager.snapshot().await;
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(rows),
    )))
}

/// `GET /api/mcp/resource`
pub async fn resource(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let manager = require_manager(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    Ok(envelope(&resolved, manager.resource_catalog().await))
}

/// `POST /api/experimental/mcp/:server/connect`
pub async fn connect(
    State(state): State<AppState>,
    AxumPath(server): AxumPath<String>,
) -> Result<Response, ErrorResponse> {
    let manager = require_manager(&state)?;
    match manager.connect(&server).await {
        Ok(_) => {
            publish_status(&state, &server, "connected");
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(error @ McpError::NotFound(_)) => {
            publish_status(&state, &server, "failed");
            Err(mcp_error(error))
        }
        Err(McpError::NotConnected { .. }) => {
            publish_status(&state, &server, "disabled");
            Err(ApiError::bad_request(format!("MCP server is disabled: {server}")).into())
        }
        Err(error) => {
            publish_status(&state, &server, "failed");
            Err(ApiError::new(502, "ProviderError", error.to_string()).into())
        }
    }
}

/// `POST /api/experimental/mcp/:server/disconnect`
pub async fn disconnect(
    State(state): State<AppState>,
    AxumPath(server): AxumPath<String>,
) -> Result<Response, ErrorResponse> {
    let manager = require_manager(&state)?;
    if manager.server(&server).is_none() {
        return Err(server_not_found(&server).into());
    }
    manager.disconnect(&server).await;
    publish_status(&state, &server, "disconnected");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `PUT /api/experimental/mcp/:server`
pub async fn put(
    State(state): State<AppState>,
    AxumPath(server): AxumPath<String>,
    Json(body): Json<Value>,
) -> Result<Response, ErrorResponse> {
    let manager = require_manager(&state)?;
    let Some(config) = body.get("config").cloned() else {
        return Err(ApiError::bad_request("missing required request field: config").into());
    };
    manager.put(&server, config).await.map_err(mcp_error)?;
    publish_status(&state, &server, "disconnected");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /api/experimental/mcp/:server`
pub async fn delete(
    State(state): State<AppState>,
    AxumPath(server): AxumPath<String>,
) -> Result<Response, ErrorResponse> {
    let manager = require_manager(&state)?;
    if manager.delete(&server).await.map_err(mcp_error)? {
        publish_status(&state, &server, "removed");
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(server_not_found(&server).into())
    }
}

/// Routes owned by this module.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/mcp", get(list))
        .route("/api/mcp/resource", get(resource))
        .route(
            "/api/experimental/mcp/{server}",
            put_route(put).delete(delete),
        )
        .route("/api/experimental/mcp/{server}/connect", post(connect))
        .route(
            "/api/experimental/mcp/{server}/disconnect",
            post(disconnect),
        )
}
