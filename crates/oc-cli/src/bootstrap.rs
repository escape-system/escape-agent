//! Server wiring: store/config/catalog, extensions, runtime and background tasks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use oc_server::AppState;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::lane::{BusLaneEvents, lane_config};
use crate::paths::default_db_path;
use crate::schedule::spawn_schedule_runner;
use crate::scribe;
use crate::service;

/// Stop everything spawned for the server, in dependency order.
pub(crate) async fn shutdown_services(
    schedule_runner: Option<JoinHandle<()>>,
    lsp: &oc_lsp::LspSupervisor,
    extensions: &Extensions,
    runtime: &oc_runtime::Runtime,
    pty: &oc_pty::PtyManager,
    broker: &oc_server::PermissionBroker,
) {
    if let Some(handle) = schedule_runner {
        handle.abort();
        let _ = handle.await;
    }
    lsp.shutdown_all().await;
    extensions.lanes.shutdown().await;
    runtime.shutdown().await;
    pty.shutdown();
    broker.reject_all("The server is shutting down.");
}

pub(crate) struct Services {
    pub(crate) store: std::sync::Arc<oc_store::Store>,
    pub(crate) config: std::sync::Arc<oc_config::ConfigService>,
    pub(crate) catalog: std::sync::Arc<oc_llm::Catalog>,
}

pub(crate) async fn open_services(
    directory: &std::path::Path,
    db: Option<PathBuf>,
) -> anyhow::Result<Services> {
    let explicit_db = db.is_some();
    let db_path = db.unwrap_or_else(default_db_path);
    let store = match oc_store::Store::open(&db_path).await {
        Ok(store) => std::sync::Arc::new(store),
        Err(error) if !explicit_db && error.to_string().contains("checksum") => {
            let fallback = db_path.with_file_name("opencode-rust.db");
            tracing::warn!(
                primary = %db_path.display(),
                fallback = %fallback.display(),
                "default database has an incompatible migration table; using opencode-rust.db"
            );
            std::sync::Arc::new(
                oc_store::Store::open(&fallback)
                    .await
                    .with_context(|| format!("open database {}", fallback.display()))?,
            )
        }
        Err(error) => {
            return Err(error).with_context(|| format!("open database {}", db_path.display()));
        }
    };
    let config = std::sync::Arc::new(
        oc_config::ConfigService::load(oc_config::LoadOptions {
            directory: directory.to_path_buf(),
            env: std::env::vars().collect(),
        })
        .context("load configuration")?,
    );
    let catalog = std::sync::Arc::new(
        oc_server::catalog_from_config(config.as_ref()).context("load model catalog")?,
    );
    Ok(Services {
        store,
        config,
        catalog,
    })
}

pub(crate) struct Extensions {
    pub(crate) lsp: Arc<oc_lsp::LspSupervisor>,
    pub(crate) lanes: Arc<oc_lanes::LaneService>,
    pub(crate) providers: Vec<Arc<dyn oc_runtime::ToolProvider>>,
}

pub(crate) async fn attach_runtime(
    state: AppState,
    services: &Services,
    extensions: &Extensions,
) -> anyhow::Result<(
    AppState,
    Arc<oc_runtime::Runtime>,
    Arc<oc_server::PermissionBroker>,
)> {
    let broker = Arc::new(oc_server::PermissionBroker::from_state(&state));
    let client = Arc::new(oc_llm::SharedLlmClient::new(
        oc_llm::LlmClient::new().context("create provider client")?,
    ));
    let mut runtime = oc_runtime::Runtime::new(oc_runtime::RuntimeConfig {
        store: services.store.clone(),
        config: services.config.clone(),
        catalog: services.catalog.clone(),
        client: Arc::clone(&client),
        tools: Arc::new(oc_tools::ToolRegistry::with_defaults()),
        sink: Arc::new(oc_server::BusSink::from_state(&state)),
        permissions: broker.clone(),
    });
    runtime = runtime.with_lanes(Arc::clone(&extensions.lanes));
    for provider in &extensions.providers {
        runtime.add_tool_provider(Arc::clone(provider));
    }
    let memory = Arc::new(oc_memory::MemoryService::new(Arc::clone(&services.store)));
    runtime = runtime.with_memory(Arc::clone(&memory));
    // One scribe serves both `digest` and the turn-end memory extraction.
    // `memory.auto_scribe` gates only the latter, inside the runtime.
    if let Some(scribe) = scribe::attach(
        services.config.as_ref(),
        services.catalog.as_ref(),
        Arc::clone(&client),
        Arc::clone(&services.store),
    ) {
        runtime = runtime.with_scribe(Arc::new(scribe));
    }
    // The binding must exist before `start`, which spawns the repair worker.
    let mut observer_engine = None;
    if let Some(handle) = oc_server::observer_host::build_for_state(
        &state,
        Arc::clone(&services.store),
        services.config.as_ref(),
        services.catalog.as_ref(),
        client,
    ) {
        let engine = Arc::clone(&handle.engine);
        observer_engine = Some(engine);
        runtime = runtime.with_observer(oc_runtime::ObserverBinding {
            engine: handle.engine,
            goals: handle.goals,
            repairs: handle.repairs,
        });
    }
    let runtime = Arc::new(runtime);
    runtime.start().await.context("start runtime")?;
    let mut state = state
        .with_runtime(runtime.clone())
        .with_permissions(broker.clone())
        .with_memory(memory);
    if let Some(engine) = observer_engine {
        state = state.with_observer_engine(engine);
    }
    Ok((state, runtime, broker))
}

pub(crate) fn attach_extensions(
    mut state: AppState,
    services: &Services,
    directory: &Path,
) -> (AppState, Extensions) {
    let mut providers: Vec<Arc<dyn oc_runtime::ToolProvider>> = Vec::new();

    match oc_server::data::mcp::McpManager::from_config(services.config.as_ref(), directory) {
        Ok(manager) => {
            state = state.with_mcp(Arc::new(manager));
        }
        Err(error) => {
            tracing::warn!(%error, "MCP servers unavailable");
        }
    }
    if let Some(manager) = state.mcp().cloned() {
        let provider: Arc<dyn oc_runtime::ToolProvider> =
            Arc::new(oc_server::data::mcp_tools::McpToolProvider::new(manager));
        providers.push(provider);
    }

    let lsp = Arc::new(oc_lsp::LspSupervisor::new(
        oc_lsp::LspConfig::from_config(services.config.merged()),
        directory.to_path_buf(),
    ));
    state = state.with_lsp(Arc::clone(&lsp));
    if let Some(supervisor) = state.lsp().cloned() {
        let provider: Arc<dyn oc_runtime::ToolProvider> =
            Arc::new(oc_server::data::lsp_tools::LspToolProvider::new(supervisor));
        providers.push(provider);
    }

    // Native plugins are compiled in, so the registry is always attached even
    // when no JS `plugins` are configured. The inventory simply stays empty.
    state = state.with_plugins(Arc::new(oc_plugin::PluginRegistry::new()));

    let runner = Arc::new(oc_runtime::ProcessLaneRunner::unbound());
    let events: Arc<dyn oc_lanes::LaneEvents> = Arc::new(BusLaneEvents {
        bus: state.bus().clone(),
        directory: directory.to_string_lossy().into_owned(),
    });
    // The service takes the erased runner; keep the concrete handle for `bind`.
    let runner_handle: Arc<dyn oc_lanes::LaneRunner> = runner.clone();
    let clock: Arc<dyn oc_lanes::Clock> = Arc::new(oc_lanes::SystemClock);
    let lanes = Arc::new(oc_lanes::LaneService::new(
        lane_config(services.config.as_ref()),
        runner_handle,
        events,
        clock,
    ));
    runner.bind(lanes.as_ref());
    state = state.with_lanes(Arc::clone(&lanes));

    (
        state,
        Extensions {
            lsp,
            lanes,
            providers,
        },
    )
}

/// Spawn the ctrl-c watcher, the managed-service registration watcher and the
/// schedule runner. Returns the runner handle so shutdown can abort it.
pub(crate) fn start_background_tasks(
    state: &AppState,
    shutdown: &CancellationToken,
    registration: Option<(&Path, &str)>,
) -> Option<JoinHandle<()>> {
    let signal = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutdown requested");
            signal.cancel();
        }
    });
    if let Some((path, id)) = registration {
        let shutdown = shutdown.clone();
        let path = path.to_path_buf();
        let id = id.to_owned();
        tokio::spawn(async move { service::watch(path, id, shutdown).await });
    }
    spawn_schedule_runner(state.clone(), shutdown.clone())
}
