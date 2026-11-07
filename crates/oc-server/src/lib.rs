//! HTTP/SSE server for opencode-rust.
//!
//! Phase 0 slice: info, location, pairing, auth, SSE connect/heartbeat and the
//! static fallback. The full route table is defined in `docs/02-protocol.md`.

pub mod auth;
pub mod bus;
pub mod data;
pub mod error;
pub mod observer_host;
pub mod pairing;
pub mod permission;
pub mod routes;
pub mod session_token;
pub mod static_files;

pub use bus::BusSink;
pub use permission::PermissionBroker;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderName, Method};
use axum::middleware;
use axum::response::Response;
use axum::routing::{get, patch as patch_route, post};
use oc_config::ConfigService;
use oc_lanes::LaneService;
use oc_llm::{Catalog, LlmError};
use oc_lsp::LspSupervisor;
use oc_memory::MemoryService;
use oc_observer::ObserverEngine;
use oc_plugin::PluginRegistry;
use oc_pty::PtyManager;
use oc_runtime::Runtime;
use oc_store::Store;
use oc_types::{EventEnvelope, ServerInfo};
use tokio::net::{TcpListener, ToSocketAddrs};
use tokio::sync::broadcast;
use tower_http::compression::CompressionLayer;
use tower_http::compression::CompressionLevel;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::data::mcp::McpManager;
use crate::data::pty::TicketStore;
use crate::pairing::PairingStore;

const BUS_CAPACITY: usize = 4096;

/// Shared server state. Cheap to clone; all heavy fields are `Arc`.
#[derive(Clone)]
pub struct AppState {
    info: ServerInfo,
    password: Option<Arc<str>>,
    pairing: Arc<PairingStore>,
    bus: broadcast::Sender<Arc<EventEnvelope>>,
    directory: Arc<PathBuf>,
    web_dist: Option<Arc<PathBuf>>,
    port: u16,
    allowed_hosts: Arc<auth::AllowedHosts>,
    cors_origins: Arc<Vec<String>>,
    store: Option<Arc<Store>>,
    config: Option<Arc<ConfigService>>,
    catalog: Option<Arc<Catalog>>,
    runtime: Option<Arc<Runtime>>,
    memory: Option<Arc<MemoryService>>,
    observer: Option<Arc<ObserverEngine>>,
    permissions: Option<Arc<PermissionBroker>>,
    pty: Option<Arc<PtyManager>>,
    mcp: Option<Arc<McpManager>>,
    lsp: Option<Arc<LspSupervisor>>,
    plugins: Option<Arc<PluginRegistry>>,
    lanes: Option<Arc<LaneService>>,
    tickets: Arc<TicketStore>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("info", &self.info)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("directory", &self.directory)
            .field("pty", &self.pty)
            .finish_non_exhaustive()
    }
}

impl AppState {
    #[must_use]
    pub fn new(info: ServerInfo, password: Option<String>, directory: PathBuf) -> Self {
        let (bus, _) = broadcast::channel(BUS_CAPACITY);
        let web_dist = std::env::var_os("OC_WEB_DIST")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .or_else(|| {
                let candidate = std::env::current_dir().ok()?.join("web").join("dist");
                candidate.is_dir().then_some(candidate)
            });
        let allowed_hosts = auth::AllowedHosts::from_bind_hostname(info.hostname.as_deref());
        Self {
            info,
            // Empty means "no authentication", matching upstream `required()`.
            password: password
                .filter(|password| !password.is_empty())
                .map(Arc::from),
            pairing: Arc::new(PairingStore::new()),
            bus,
            directory: Arc::new(directory),
            web_dist: web_dist.map(Arc::new),
            port: 0,
            allowed_hosts: Arc::new(allowed_hosts),
            cors_origins: Arc::new(Vec::new()),
            store: None,
            config: None,
            catalog: None,
            runtime: None,
            memory: None,
            observer: None,
            permissions: None,
            pty: None,
            mcp: None,
            lsp: None,
            plugins: None,
            lanes: None,
            tickets: Arc::new(TicketStore::new()),
        }
    }

    /// Additional allowed CORS origins (exact match), beyond loopback.
    #[must_use]
    pub fn with_cors_origins(mut self, origins: Vec<String>) -> Self {
        self.cors_origins = Arc::new(origins);
        self
    }

    #[must_use]
    pub fn cors_origins(&self) -> &[String] {
        &self.cors_origins
    }

    /// Set the bound port (used in the session cookie name).
    #[must_use]
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Host names accepted in the `Host` header (loopback plus the bind name).
    #[must_use]
    pub fn allowed_hosts(&self) -> &auth::AllowedHosts {
        &self.allowed_hosts
    }

    /// Attach the persistence layer used by the data routes.
    #[must_use]
    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    /// Attach the configuration snapshot used by the data routes.
    #[must_use]
    pub fn with_config(mut self, config: Arc<ConfigService>) -> Self {
        self.config = Some(config);
        self
    }

    /// Attach the model catalog used by the model/provider routes.
    ///
    /// Build it once at startup with [`catalog_from_config`]; re-parsing the
    /// embedded snapshot per request would be wasteful.
    #[must_use]
    pub fn with_catalog(mut self, catalog: Arc<Catalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    #[must_use]
    pub fn store(&self) -> Option<&Arc<Store>> {
        self.store.as_ref()
    }

    #[must_use]
    pub fn config(&self) -> Option<&Arc<ConfigService>> {
        self.config.as_ref()
    }

    #[must_use]
    pub fn catalog(&self) -> Option<&Arc<Catalog>> {
        self.catalog.as_ref()
    }

    /// Attach the session runtime. Handlers only admit input and signal it;
    /// model execution always happens inside the runtime.
    #[must_use]
    pub fn with_runtime(mut self, runtime: Arc<Runtime>) -> Self {
        self.runtime = Some(runtime);
        self
    }

    #[must_use]
    pub fn runtime(&self) -> Option<&Arc<Runtime>> {
        self.runtime.as_ref()
    }

    /// Attach the memory service used by the experimental memory routes.
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<MemoryService>) -> Self {
        self.memory = Some(memory);
        self
    }

    #[must_use]
    pub fn memory(&self) -> Option<&Arc<MemoryService>> {
        self.memory.as_ref()
    }

    /// Attach the observer engine used by the experimental audit routes.
    ///
    /// CLI wiring: `state.with_observer_engine(Arc::clone(&handle.engine))`
    /// where `handle` is the [`observer_host::ObserverHandle`].
    #[must_use]
    pub fn with_observer_engine(mut self, observer: Arc<ObserverEngine>) -> Self {
        self.observer = Some(observer);
        self
    }

    #[must_use]
    pub fn observer(&self) -> Option<&Arc<ObserverEngine>> {
        self.observer.as_ref()
    }

    /// Attach the broker the runtime uses to resolve `ask` permissions.
    #[must_use]
    pub fn with_permissions(mut self, broker: Arc<PermissionBroker>) -> Self {
        self.permissions = Some(broker);
        self
    }

    #[must_use]
    pub fn permissions(&self) -> Option<&Arc<PermissionBroker>> {
        self.permissions.as_ref()
    }

    /// Attach the in-process PTY manager serving `/api/pty`.
    #[must_use]
    pub fn with_pty(mut self, pty: Arc<PtyManager>) -> Self {
        self.pty = Some(pty);
        self
    }

    #[must_use]
    pub fn pty(&self) -> Option<&Arc<PtyManager>> {
        self.pty.as_ref()
    }

    /// Attach the MCP manager serving `/api/mcp`.
    ///
    /// CLI wiring:
    /// `state.with_mcp(Arc::new(McpManager::from_config(services.config.as_ref(), &directory)?))`.
    /// Nothing connects until `POST /api/experimental/mcp/:server/connect`.
    #[must_use]
    pub fn with_mcp(mut self, manager: Arc<McpManager>) -> Self {
        self.mcp = Some(manager);
        self
    }

    #[must_use]
    pub fn mcp(&self) -> Option<&Arc<McpManager>> {
        self.mcp.as_ref()
    }

    /// Attach the LSP supervisor serving `/api/lsp`.
    ///
    /// CLI wiring:
    /// `state.with_lsp(Arc::new(LspSupervisor::new(LspConfig::from_config(services.config.merged()), directory.clone())))`,
    /// or `LspSupervisor::with_options(...)` for custom tuning. Servers spawn
    /// lazily on file use; the status route never forces a spawn.
    ///
    /// The supervisor's event stream is bridged onto the SSE bus here. The task
    /// holds only the broadcast receiver, so it ends when the last supervisor
    /// handle is dropped.
    #[must_use]
    pub fn with_lsp(mut self, supervisor: Arc<LspSupervisor>) -> Self {
        crate::data::lsp::bridge_events(&supervisor, &self.bus);
        self.lsp = Some(supervisor);
        self
    }

    #[must_use]
    pub fn lsp(&self) -> Option<&Arc<LspSupervisor>> {
        self.lsp.as_ref()
    }

    /// Attach the native plugin registry serving `/api/plugin`.
    ///
    /// CLI wiring: register built-ins on `PluginRegistry::new()` first, then
    /// `state.with_plugins(Arc::new(registry))`. The JS sidecar is not started
    /// by the server.
    #[must_use]
    pub fn with_plugins(mut self, registry: Arc<PluginRegistry>) -> Self {
        self.plugins = Some(registry);
        self
    }

    #[must_use]
    pub fn plugins(&self) -> Option<&Arc<PluginRegistry>> {
        self.plugins.as_ref()
    }

    /// Attach the build-lane service serving `/api/experimental/lane` and the
    /// lane-aware shell tool.
    #[must_use]
    pub fn with_lanes(mut self, lanes: Arc<LaneService>) -> Self {
        self.lanes = Some(lanes);
        self
    }

    #[must_use]
    pub fn lanes(&self) -> Option<&Arc<LaneService>> {
        self.lanes.as_ref()
    }

    /// Short-lived single-use WebSocket tickets.
    pub(crate) fn tickets(&self) -> &TicketStore {
        &self.tickets
    }

    /// Override the web build directory (used by tests and `--web-dist`).
    #[must_use]
    pub fn with_web_dist(mut self, path: impl Into<PathBuf>) -> Self {
        self.web_dist = Some(Arc::new(path.into()));
        self
    }

    #[must_use]
    pub fn web_dist(&self) -> Option<&Path> {
        self.web_dist.as_deref().map(PathBuf::as_path)
    }

    /// Serve a static asset if a web build is configured.
    pub async fn static_response(&self, method: &Method, path: &str) -> Option<Response> {
        static_files::serve(self, method, path).await
    }

    #[must_use]
    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    #[must_use]
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    #[must_use]
    pub fn pairing(&self) -> &PairingStore {
        &self.pairing
    }

    #[must_use]
    pub fn bus(&self) -> &broadcast::Sender<Arc<EventEnvelope>> {
        &self.bus
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Publish an event to every connected SSE subscriber.
    pub fn publish(&self, event: EventEnvelope) {
        let _ = self.bus.send(Arc::new(event));
    }
}

/// Build the model catalog from the embedded models.dev snapshot plus the
/// merged `providers` configuration object.
///
/// Call once at startup and attach it with [`AppState::with_catalog`]; parsing
/// the 7.9 MB snapshot is too expensive for a request handler.
pub fn catalog_from_config(config: &ConfigService) -> Result<Catalog, LlmError> {
    let mut catalog = Catalog::from_snapshot(oc_llm::catalog::EMBEDDED_SNAPSHOT)?;
    if let Some(providers) = config.merged().get("providers") {
        catalog.apply_config(providers)?;
    }
    Ok(catalog)
}

/// Build the full router.
#[allow(clippy::too_many_lines)] // the route table is one linear list
pub fn router(state: AppState) -> Router {
    let public = Router::new()
        .route("/auth/connect/{code}", get(routes::connect))
        // The WebSocket handshake authenticates with a single-use ticket, not
        // Basic auth, so it bypasses the protected layer like upstream.
        .route("/api/pty/{id}/connect", get(data::pty::connect))
        .route(
            "/api/experimental/persistent-pty/{id}/connect",
            get(data::pty::connect),
        );

    let protected = Router::new()
        // Minting a pairing code hands out a session token, so it must not be
        // reachable without the password.
        .route("/api/pair", post(routes::pair))
        .route("/api/info", get(routes::info))
        .route("/api/location", get(routes::location))
        .route("/api/event", get(routes::events))
        .route("/api/pty", get(data::pty::list).post(data::pty::create))
        .route(
            "/api/pty/{id}",
            get(data::pty::get)
                .put(data::pty::update)
                .delete(data::pty::remove),
        )
        .route(
            "/api/pty/{id}/connect-token",
            post(data::pty::connect_token),
        )
        .route(
            "/api/session",
            get(data::session::list).post(data::session::create),
        )
        .route("/api/session/active", get(data::session::active))
        .route(
            "/api/session/{sid}",
            get(data::session::get)
                .patch(data::session::patch)
                .delete(data::session::delete),
        )
        .route(
            "/api/session/{sid}/agent",
            post(data::session::select_agent),
        )
        .route(
            "/api/session/{sid}/model",
            post(data::session::select_model),
        )
        .route("/api/session/{sid}/move", post(data::session::move_session))
        .route(
            "/api/session/{sid}/interrupt",
            post(data::session::interrupt),
        )
        .route(
            "/api/session/{sid}/background",
            post(data::session::background),
        )
        .route("/api/session/{sid}/prompt", post(data::session::prompt))
        .route(
            "/api/session/{sid}/synthetic",
            post(data::session::synthetic),
        )
        .route("/api/session/{sid}/compact", post(data::session::compact))
        .route("/api/session/{sid}/command", post(data::session::command))
        .route(
            "/api/session/{sid}/permission",
            get(data::permission::list).post(data::permission::evaluate),
        )
        .route(
            "/api/session/{sid}/permission/{request_id}",
            get(data::permission::get),
        )
        .route(
            "/api/session/{sid}/permission/{request_id}/reply",
            post(data::permission::reply),
        )
        .route(
            "/api/experimental/session/{sid}/wait",
            post(data::session::wait),
        )
        .route("/api/permission/request", get(data::permission::pending))
        .route("/api/skill", get(data::config::skills))
        .route("/api/command", get(data::config::commands))
        .route("/api/reference", get(data::config::references))
        .route("/api/session/{sid}/message", get(data::message::list))
        .route("/api/session/{sid}/message/{mid}", get(data::message::get))
        .route("/api/session/{sid}/inbox", get(data::inbox::list))
        .route(
            "/api/session/{sid}/inbox/{id}",
            axum::routing::patch(data::inbox::update).delete(data::inbox::delete),
        )
        .route("/api/session/{sid}/log", get(data::log::stream))
        .route(
            "/api/experimental/session/{sid}/log",
            get(data::log::stream),
        )
        .route("/api/config", get(data::config::entries))
        .route("/api/config/shell", get(data::config::shells))
        .route(
            "/api/experimental/config",
            patch_route(data::config_patch::patch),
        )
        .route("/api/agent", get(data::config::agents))
        .route("/api/agent/{id}", get(data::config::agent))
        .route("/api/model", get(data::model::models))
        .route("/api/model/default", get(data::model::default_model))
        .route("/api/provider", get(data::model::providers))
        .route("/api/provider/{id}", get(data::model::provider))
        .route("/api/project", get(data::project::list))
        .route(
            "/api/project/{id}",
            axum::routing::patch(data::project::update),
        )
        .route("/api/vcs", get(data::vcs::info))
        .route("/api/vcs/base", get(data::vcs::base))
        .route("/api/vcs/status", get(data::vcs::status))
        .route("/api/vcs/branch", get(data::vcs::branches))
        .route("/api/vcs/diff", get(data::vcs::diff))
        .route("/api/vcs/init", post(data::vcs::init))
        .route("/api/form", get(data::lists::forms))
        .route(
            "/api/experimental/usage/limits",
            get(data::lists::usage_limits),
        )
        .route(
            "/api/websearch/provider",
            get(data::lists::websearch_providers),
        )
        .route("/api/fs/read/{*path}", get(data::fs::read))
        .route("/api/fs/list", get(data::fs::list))
        .route("/api/fs/find", get(data::fs::find))
        .route(
            "/api/experimental/memory",
            get(data::memory::list).post(data::memory::write),
        )
        .route(
            "/api/experimental/memory/{id}",
            axum::routing::delete(data::memory::forget),
        )
        .route(
            "/api/experimental/goal",
            get(data::goal::get).post(data::goal::set),
        )
        .route(
            "/api/experimental/goal/{id}",
            axum::routing::patch(data::goal::patch),
        )
        .route("/api/experimental/goal/history", get(data::goal::history))
        .route("/api/experimental/subagent/tree", get(data::subagent::tree))
        .route(
            "/api/experimental/subagent/{sid}/message",
            post(data::subagent::message),
        )
        .merge(data::session_ext::routes())
        .merge(data::shells::routes())
        .merge(data::experimental::routes())
        .merge(data::mcp::routes())
        .merge(data::lsp::routes())
        .merge(data::plugin::routes())
        .merge(data::lane::routes())
        .merge(data::integration::routes())
        .merge(data::worktree::routes())
        .merge(data::observer::routes())
        .merge(data::schedule::routes())
        .layer(middleware::from_fn_with_state(state.clone(), auth::require));

    // Compression covers the static shell only. `/api` is deliberately outside
    // this router: SSE must never be compressed or buffered (`docs/13-ui.md`),
    // and JSON API payloads are not the first-load cost this targets.
    //
    // The fallback is a custom fallback, so it must live on exactly one router:
    // `axum` panics when two routers with custom fallbacks are merged. The API
    // routers below only add routes (`Router::new()` default fallback), which
    // `Router::merge` adopts without conflict.
    let statics = Router::new().fallback(routes::fallback).layer(
        CompressionLayer::new()
            // `Fastest` keeps the 34 MiB of wasm modules from turning a
            // static request into a CPU amplifier.
            .quality(CompressionLevel::Fastest)
            .compress_when(
                SizeAbove::new(1024)
                    .and(NotForContentType::SSE)
                    .and(NotForContentType::IMAGES)
                    .and(NotForContentType::new("font/"))
                    .and(NotForContentType::new("audio/"))
                    .and(NotForContentType::new("video/")),
            ),
    );

    statics
        .merge(public)
        .merge(protected)
        .layer(middleware::from_fn_with_state(state.clone(), inject_state))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::host_guard,
        ))
        .layer(cors(&state))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Make the state available to the request-only fallback handler.
async fn inject_state(
    State(state): State<AppState>,
    mut req: Request,
    next: middleware::Next,
) -> Response {
    req.extensions_mut().insert(state);
    next.run(req).await
}

fn cors(state: &AppState) -> CorsLayer {
    let extra = state.cors_origins().to_vec();
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            origin.to_str().is_ok_and(|origin| {
                origin.starts_with("http://localhost:")
                    || origin.starts_with("http://127.0.0.1:")
                    || origin.starts_with("http://[::1]:")
                    || origin == "oc://renderer"
                    || origin == "tauri://localhost"
                    || extra.iter().any(|allowed| allowed == origin)
            })
        }))
        .allow_methods(AllowMethods::list([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ]))
        .allow_headers(AllowHeaders::list([
            AUTHORIZATION,
            CONTENT_TYPE,
            HeaderName::from_static("x-opencode-directory"),
            HeaderName::from_static("x-opencode-capabilities"),
            HeaderName::from_static("x-opencode-ticket"),
        ]))
        .allow_credentials(true)
        .max_age(Duration::from_secs(86_400))
}

/// Bind a TCP listener.
pub async fn bind(address: impl ToSocketAddrs) -> std::io::Result<TcpListener> {
    TcpListener::bind(address).await
}

/// Serve until the process is stopped.
pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}
