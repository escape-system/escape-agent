//! Server wiring: store/config/catalog, extensions, runtime and background tasks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use oc_server::AppState;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::lane::{BusLaneEvents, StoreLaneTickets, lane_config};
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
    pub(crate) catalog: std::sync::Arc<oc_llm::CatalogCell>,
    pub(crate) antigravity: Option<Arc<oc_antigravity::AntigravityService>>,
}

/// Open the SQLite store with the legacy checksum-mismatch fallback.
pub(crate) async fn open_store(db: Option<PathBuf>) -> anyhow::Result<Arc<oc_store::Store>> {
    let explicit_db = db.is_some();
    let db_path = db.unwrap_or_else(default_db_path);
    match oc_store::Store::open(&db_path).await {
        Ok(store) => Ok(Arc::new(store)),
        Err(error) if !explicit_db && error.to_string().contains("checksum") => {
            let fallback = db_path.with_file_name("opencode-rust.db");
            tracing::warn!(
                primary = %db_path.display(),
                fallback = %fallback.display(),
                "default database has an incompatible migration table; using opencode-rust.db"
            );
            Ok(Arc::new(
                oc_store::Store::open(&fallback)
                    .await
                    .with_context(|| format!("open database {}", fallback.display()))?,
            ))
        }
        Err(error) => Err(error).with_context(|| format!("open database {}", db_path.display())),
    }
}

pub(crate) async fn open_services(
    directory: &std::path::Path,
    db: Option<PathBuf>,
) -> anyhow::Result<Services> {
    let store = open_store(db).await?;
    let config = std::sync::Arc::new(
        oc_config::ConfigService::load(oc_config::LoadOptions {
            directory: directory.to_path_buf(),
            env: std::env::vars().collect(),
        })
        .context("load configuration")?,
    );
    let mut catalog =
        oc_server::catalog_from_config(config.as_ref()).context("load model catalog")?;
    // `config_dir: None` is deliberate: Antigravity account, signature and
    // model-cache state is global (`resolve_config_dir()`), so every project
    // shares one account pool; only `antigravity.jsonc` discovery is per-project.
    let antigravity = match oc_antigravity::AntigravityService::open(
        Some(Arc::clone(&store)),
        None,
        Some(directory.to_path_buf()),
        oc_antigravity::AntigravityEndpoints::default(),
    )
    .await
    {
        Ok(svc) => {
            svc.overlay_catalog(&mut catalog);
            Some(Arc::new(svc))
        }
        Err(error) => {
            tracing::warn!(%error, "Antigravity service unavailable");
            None
        }
    };
    // models.dev is a dynamic source, not a build artifact: a catalog fetched by
    // a previous run *replaces* the embedded floor, then the same overlays the
    // embedded path uses are re-applied. An implausible or unparseable cache is
    // ignored, never served, so a truncated file cannot empty the picker.
    match oc_llm::catalog::refresh::read_cache(&oc_llm::catalog::refresh::cache_path()) {
        Some(Ok(body)) => match oc_llm::catalog::refresh::parse_live(&body) {
            Ok(live) if oc_llm::catalog::refresh::is_plausible(&live) => {
                catalog = live;
                if let Some(providers) = config.merged().get("providers") {
                    catalog
                        .apply_config(providers)
                        .context("apply providers to the cached catalog")?;
                }
            }
            Ok(live) => tracing::warn!(
                models = live.models().len(),
                "cached model catalog too small; using the embedded snapshot"
            ),
            Err(error) => tracing::warn!(%error, "cached model catalog unparseable"),
        },
        Some(Err(error)) => tracing::warn!(%error, "cached model catalog unreadable"),
        None => {}
    }
    let catalog = std::sync::Arc::new(oc_llm::CatalogCell::new(catalog));
    Ok(Services {
        store,
        config,
        catalog,
        antigravity,
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
    if let Some(antigravity) = services.antigravity.clone() {
        let hook: Arc<dyn oc_llm::LlmTransportHook> = antigravity;
        client.set_hook(Some(hook));
    }
    let mut runtime = oc_runtime::Runtime::new(oc_runtime::RuntimeConfig {
        store: services.store.clone(),
        config: services.config.clone(),
        catalog: services.catalog.clone(),
        client: Arc::clone(&client),
        tools: Arc::new(oc_tools::ToolRegistry::with_defaults()),
        sink: Arc::new(oc_server::BusSink::from_state(&state)),
        permissions: broker.clone(),
    });
    runtime = runtime
        .with_lanes(Arc::clone(&extensions.lanes))
        .with_edit_diagnostics(Arc::new(
            oc_server::data::lsp_tools::LspEditDiagnosticsHook::new(Arc::clone(&extensions.lsp)),
        ))
        .with_semantic_oracle(Arc::new(
            oc_server::data::lsp_tools::LspSemanticOracle::new(Arc::clone(&extensions.lsp)),
        ));
    for provider in &extensions.providers {
        runtime.add_tool_provider(Arc::clone(provider));
    }
    let memory = Arc::new(oc_memory::MemoryService::new(Arc::clone(&services.store)));
    runtime = runtime.with_memory(Arc::clone(&memory));
    // One scribe serves both `digest` and the turn-end memory extraction.
    // `memory.auto_scribe` gates only the latter, inside the runtime.
    let catalog_generation = services.catalog.current();
    if let Some(scribe) = scribe::attach(
        services.config.as_ref(),
        catalog_generation.as_ref(),
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
        catalog_generation.as_ref(),
        Arc::clone(&client),
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
        .with_memory(memory)
        .with_llm(Arc::clone(&client));
    if let Some(engine) = observer_engine {
        state = state.with_observer_engine(engine);
    }
    Ok((state, runtime, broker))
}

/// Builds the MCP/LSP/plugin/lane extensions and attaches them to the state.
///
/// The lane service is restored from the persisted `lane_ticket` table: queued
/// rows are re-enqueued in their original order, `running` rows are failed as
/// interrupted by the restart, and the id counter resumes above the highest
/// restored id. A restore failure is logged and propagated, never swallowed:
/// booting with a silently empty queue would lose submitted work.
pub(crate) async fn attach_extensions(
    mut state: AppState,
    services: &Services,
    directory: &Path,
) -> anyhow::Result<(AppState, Extensions)> {
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

    if let Some(antigravity) = services.antigravity.clone() {
        antigravity.spawn_background_version_check();
        // Keeps cached quota fresh so the soft-quota gate is live at request time, and
        // refreshes tokens ahead of expiry. Returns false and spawns nothing when disabled.
        antigravity.spawn_background_maintenance();
        state = state.with_antigravity(Arc::clone(&antigravity));
        let provider: Arc<dyn oc_runtime::ToolProvider> =
            Arc::new(oc_server::data::antigravity_tools::AntigravityToolProvider::new(antigravity));
        providers.push(provider);
    }

    // Native plugins are compiled in, so the registry is always attached even
    // when no JS `plugins` are configured. The inventory simply stays empty.
    state = state.with_plugins(Arc::new(oc_plugin::PluginRegistry::new()));

    let runner = Arc::new(oc_runtime::ProcessLaneRunner::unbound());
    let events: Arc<dyn oc_lanes::LaneEvents> = Arc::new(BusLaneEvents {
        bus: state.bus().clone(),
        directory: directory.to_string_lossy().into_owned(),
        store: Some(Arc::clone(&services.store)),
    });
    // The service takes the erased runner; keep the concrete handle for `bind`.
    let runner_handle: Arc<dyn oc_lanes::LaneRunner> = runner.clone();
    let clock: Arc<dyn oc_lanes::Clock> = Arc::new(oc_lanes::SystemClock);
    let lanes = Arc::new(
        oc_lanes::LaneService::restore(
            lane_config(services.config.as_ref()),
            runner_handle,
            events,
            clock,
            Arc::new(StoreLaneTickets::new(Arc::clone(&services.store))),
        )
        .await
        .map_err(|error| {
            tracing::error!(%error, "lane restore failed");
            anyhow::Error::new(error)
        })
        .context("restore lane service")?,
    );
    runner.bind(lanes.as_ref());
    state = state.with_lanes(Arc::clone(&lanes));

    Ok((
        state,
        Extensions {
            lsp,
            lanes,
            providers,
        },
    ))
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
    if let Some(cell) = state.catalog_cell() {
        // A published catalog is authoritative, so the swap rebuilds the whole
        // generation: parse, gate on plausibility, re-apply the same overlays the
        // boot path applies, then replace and announce. Dropping the handle
        // detaches the loop; shutdown ends the process.
        let cell = std::sync::Arc::clone(cell);
        let state = state.clone();
        let _refresh = oc_llm::catalog::refresh::spawn_refresh_loop(move |body| {
            let mut live = oc_llm::catalog::refresh::parse_live(body).map_err(|e| e.to_string())?;
            if !oc_llm::catalog::refresh::is_plausible(&live) {
                return Err(format!(
                    "published catalog has only {} models",
                    live.models().len()
                ));
            }
            if let Some(config) = state.config()
                && let Some(providers) = config.merged().get("providers")
            {
                live.apply_config(providers).map_err(|e| e.to_string())?;
            }
            if let Some(antigravity) = state.antigravity() {
                antigravity.overlay_catalog(&mut live);
            }
            cell.replace(live);
            state.publish(oc_types::EventEnvelope::now(
                "models-dev.refreshed",
                serde_json::json!({}),
            ));
            Ok(())
        });
    }
    spawn_schedule_runner(state.clone(), shutdown.clone())
}
