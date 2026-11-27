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

pub(crate) fn antigravity_legacy_oauth_enabled(config: &oc_config::ConfigService) -> bool {
    let env_enabled = std::env::var("OC_ALLOW_ANTIGRAVITY_OAUTH").is_ok_and(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    });
    let cfg_enabled = config
        .merged()
        .pointer("/antigravity/allow_legacy_consumer_oauth")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    env_enabled || cfg_enabled
}

fn configure_provider_rate_limits(client: &oc_llm::LlmClient, config: &oc_config::ConfigService) {
    let Some(providers) = config
        .merged()
        .get("providers")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    for (provider_id, entry) in providers {
        let Some(rl) = entry
            .get("rate_limit")
            .or_else(|| entry.get("rateLimit"))
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        let rpm = rl
            .get("rpm")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let tpm = rl
            .get("tpm")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let spend_cap = rl
            .get("spend_cap_10m_usd")
            .or_else(|| rl.get("spendCap10mUsd"))
            .and_then(serde_json::Value::as_f64);
        let max_conc = rl
            .get("max_concurrent")
            .or_else(|| rl.get("maxConcurrent"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok());
        if rpm > 0 || tpm > 0 || spend_cap.is_some() || max_conc.is_some() {
            let mut policy = oc_llm::RateLimitPolicy::new(rpm, tpm, spend_cap);
            policy.max_concurrent = max_conc;
            client.set_rate_limit_policy(provider_id, policy);
        }
    }
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
    // models.dev is a dynamic source, not a build artifact: a plausible cached
    // catalog replaces the embedded snapshot at startup, so avoid parsing the
    // 7.9 MB embedded snapshot first only to immediately drop it.
    let cached_live =
        match oc_llm::catalog::refresh::read_cache(&oc_llm::catalog::refresh::cache_path()) {
            Some(Ok(body)) => match oc_llm::catalog::refresh::parse_live(&body) {
                Ok(mut live) if oc_llm::catalog::refresh::is_plausible(&live) => {
                    oc_llm::catalog::refresh::apply_builtin_providers(&mut live);
                    apply_cached_nous_models(&mut live);
                    apply_cached_opencode_go_models(&mut live);
                    if let Some(providers) = config.merged().get("providers") {
                        live.apply_config(providers)
                            .context("apply providers to the cached catalog")?;
                    }
                    Some(live)
                }
                Ok(live) => {
                    tracing::warn!(
                        models = live.models().len(),
                        "cached model catalog too small; using the embedded snapshot"
                    );
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "cached model catalog unparseable");
                    None
                }
            },
            Some(Err(error)) => {
                tracing::warn!(%error, "cached model catalog unreadable");
                None
            }
            None => None,
        };

    let mut catalog = if let Some(live) = cached_live {
        live
    } else {
        let mut embedded =
            oc_server::catalog_from_config(config.as_ref()).context("load model catalog")?;
        let has_nous_cache = apply_cached_nous_models(&mut embedded);
        let has_go_cache = apply_cached_opencode_go_models(&mut embedded);
        if (has_nous_cache || has_go_cache)
            && let Some(providers) = config.merged().get("providers")
        {
            embedded
                .apply_config(providers)
                .context("reapply providers over cached provider models")?;
        }
        embedded
    };
    // Consumer Antigravity OAuth proxy and unattended Cloud Code polling carry a
    // direct Article 6 ToS ban risk on consumer plans (`docs/30-tos-safe-harness.md`),
    // so attachment stays completely disabled unless explicitly opted in.
    let antigravity = if antigravity_legacy_oauth_enabled(config.as_ref()) {
        tracing::warn!(
            "legacy consumer Antigravity OAuth gateway enabled via opt-in; \
             prefer Paid Gemini API (`GEMINI_API_KEY`), Vertex AI (`google-vertex`), or `opencode mcp` to avoid ToS bans"
        );
        match oc_antigravity::AntigravityService::open(
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
        }
    } else {
        None
    };
    let catalog = std::sync::Arc::new(oc_llm::CatalogCell::new(catalog));
    oc_alloc::purge_idle_memory();
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
    // Single-owner retry rule (`docs/30-tos-safe-harness.md`): Runtime owns
    // retries & key rotation so LlmClient never multiplies attempts beneath it.
    let raw_client = oc_llm::LlmClient::new()
        .context("create provider client")?
        .with_retry_policy(oc_llm::RetryPolicy::single_attempt());
    configure_provider_rate_limits(&raw_client, services.config.as_ref());
    let client = Arc::new(oc_llm::SharedLlmClient::new(raw_client));
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

    let pruned = oc_runtime::cleanup_stale_lane_logs(std::time::Duration::from_secs(24 * 3600));
    if pruned > 0 {
        tracing::debug!(removed = pruned, "cleaned stale lane log files");
    }

    let runner = Arc::new(oc_runtime::ProcessLaneRunner::unbound());
    let events: Arc<dyn oc_lanes::LaneEvents> = Arc::new(BusLaneEvents::new(
        state.bus().clone(),
        directory.to_string_lossy().into_owned(),
        Some(Arc::clone(&services.store)),
    ));
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
        wait_for_shutdown_signal().await;
        tracing::info!("shutdown requested");
        signal.cancel();
    });
    if let Some((path, id)) = registration {
        let shutdown = shutdown.clone();
        let path = path.to_path_buf();
        let id = id.to_owned();
        tokio::spawn(async move { service::watch(path, id, shutdown).await });
    }
    if let Some(cell) = state.catalog_cell() {
        spawn_catalog_refresh_loop(std::sync::Arc::clone(cell), state.clone(), shutdown.clone());
    }
    spawn_schedule_runner(state.clone(), shutdown.clone())
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sigterm) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn apply_cached_nous_models(catalog: &mut oc_llm::Catalog) -> bool {
    let Some(Ok(body)) =
        oc_llm::catalog::refresh::read_cache(&oc_llm::catalog::refresh::nous_cache_path())
    else {
        return false;
    };
    let Ok(models) = oc_llm::catalog::refresh::parse_nous_models(&body) else {
        return false;
    };
    catalog.upsert_provider(oc_llm::catalog::refresh::default_nous_provider());
    catalog.upsert_provider_models(models);
    true
}

/// Apply the cached `opencode-go` provider list, so a model the subscription
/// added since the last boot is served before the first network fetch.
///
/// Membership is part of the cache: a restarting process must also stop
/// offering ids the provider dropped, which is why this goes through
/// [`oc_llm::catalog::refresh::apply_provider_models`] instead of a bare
/// upsert.
fn apply_cached_opencode_go_models(catalog: &mut oc_llm::Catalog) -> bool {
    let Some(Ok(body)) =
        oc_llm::catalog::refresh::read_cache(&oc_llm::catalog::refresh::opencode_go_cache_path())
    else {
        return false;
    };
    let Ok(models) = oc_llm::catalog::refresh::parse_provider_models(
        &body,
        oc_llm::catalog::refresh::OPENCODE_GO_PROVIDER_ID,
    ) else {
        return false;
    };
    oc_llm::catalog::refresh::apply_provider_models(
        catalog,
        oc_llm::catalog::refresh::OPENCODE_GO_PROVIDER_ID,
        models,
    );
    true
}

/// Run all background catalog refreshes (`models.dev`, `opencode-go`, `nous`) in
/// one serialized loop so concurrent `Catalog` clones cannot spike peak RSS and
/// no idle HTTP/TLS client pool stays resident between 6-hour ticks.
fn spawn_catalog_refresh_loop(
    cell: Arc<oc_llm::CatalogCell>,
    state: AppState,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut first = true;
        loop {
            let delay = if first {
                oc_llm::catalog::refresh::BOOT_DELAY
            } else {
                oc_llm::catalog::refresh::REFRESH_INTERVAL
            };
            first = false;
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shutdown.cancelled() => break,
            }
            refresh_catalog_once(&cell, &state).await;
            // Let dropped HTTP driver tasks settle before sweeping heaps back to the OS.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            oc_alloc::purge_idle_memory();
        }
    });
}

async fn fetch_refresh_payloads(
    models_url: &str,
) -> (
    Option<oc_llm::Catalog>,
    Option<Vec<oc_llm::ModelInfo>>,
    Option<Vec<oc_llm::ModelInfo>>,
) {
    let models_cache = oc_llm::catalog::refresh::cache_path();
    let go_url = oc_llm::catalog::refresh::opencode_go_models_url();
    let go_cache = oc_llm::catalog::refresh::opencode_go_cache_path();
    let client = oc_llm::catalog::refresh::transient_client();
    let live_catalog = match oc_llm::catalog::refresh::fetch_live(&client, models_url).await {
        Ok(body) => {
            if let Err(error) = oc_llm::catalog::refresh::write_cache(&models_cache, &body) {
                tracing::warn!(%error, "model catalog cache write failed");
            }
            match oc_llm::catalog::refresh::parse_live(&body) {
                Ok(live) if oc_llm::catalog::refresh::is_plausible(&live) => Some(live),
                Ok(live) => {
                    tracing::warn!(
                        models = live.models().len(),
                        "model catalog publish rejected: too few models"
                    );
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "model catalog publish rejected");
                    None
                }
            }
        }
        Err(error) => {
            tracing::debug!(%error, "model catalog refresh failed");
            None
        }
    };
    let go_models = match oc_llm::catalog::refresh::fetch_opencode_go_models_with_body(
        &client, &go_url,
    )
    .await
    {
        Ok((body, models)) => {
            if let Err(error) = oc_llm::catalog::refresh::write_cache(&go_cache, &body) {
                tracing::debug!(%error, "opencode-go model list cache write failed");
            }
            Some(models)
        }
        Err(error) => {
            tracing::debug!(%error, "opencode-go model list refresh failed");
            None
        }
    };
    let nous_models = match oc_llm::catalog::refresh::fetch_nous_models_now(&client).await {
        Ok((body, models)) => {
            if let Err(error) = oc_llm::catalog::refresh::write_cache(
                &oc_llm::catalog::refresh::nous_cache_path(),
                &body,
            ) {
                tracing::debug!(%error, "nous catalog cache write failed");
            }
            Some(models)
        }
        Err(error) => {
            tracing::debug!(%error, "nous model catalog refresh failed");
            None
        }
    };
    (live_catalog, go_models, nous_models)
}

async fn refresh_catalog_once(cell: &Arc<oc_llm::CatalogCell>, state: &AppState) {
    let models_url = oc_llm::catalog::refresh::models_url();
    // Client lives strictly inside `fetch_refresh_payloads` so TLS/pool state drops before catalog assembly.
    let (live_catalog, go_models, nous_models) = fetch_refresh_payloads(&models_url).await;

    if live_catalog.is_none() && go_models.is_none() && nous_models.is_none() {
        return;
    }

    let mut next = if let Some(mut live) = live_catalog {
        oc_llm::catalog::refresh::apply_builtin_providers(&mut live);
        if go_models.is_none() {
            apply_cached_opencode_go_models(&mut live);
        }
        if nous_models.is_none() {
            apply_cached_nous_models(&mut live);
        }
        tracing::info!("model catalog refreshed from {models_url}");
        live
    } else {
        cell.current().as_ref().clone()
    };

    if let Some(models) = go_models {
        let (upserted, removed) = oc_llm::catalog::refresh::apply_provider_models(
            &mut next,
            oc_llm::catalog::refresh::OPENCODE_GO_PROVIDER_ID,
            models,
        );
        tracing::info!(upserted, removed, "opencode-go model list refreshed");
    }
    if let Some(models) = nous_models {
        next.upsert_provider(oc_llm::catalog::refresh::default_nous_provider());
        next.upsert_provider_models(models);
    }

    if let Some(config) = state.config()
        && let Some(providers) = config.merged().get("providers")
        && let Err(error) = next.apply_config(providers)
    {
        tracing::warn!(%error, "catalog config reapply failed");
        return;
    }
    if let Some(antigravity) = state.antigravity() {
        antigravity.overlay_catalog(&mut next);
    }
    cell.replace(next);
    state.publish(oc_types::EventEnvelope::now(
        "models-dev.refreshed",
        serde_json::json!({}),
    ));
}

#[cfg(test)]
mod tests {
    use super::antigravity_legacy_oauth_enabled;

    #[test]
    fn antigravity_legacy_oauth_defaults_false_and_requires_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = oc_config::ConfigService::load(oc_config::LoadOptions {
            directory: dir.path().to_path_buf(),
            env: std::collections::HashMap::new(),
        })
        .unwrap();
        assert!(!antigravity_legacy_oauth_enabled(&cfg));

        let opt_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            opt_dir.path().join("opencode.json"),
            r#"{"antigravity":{"allow_legacy_consumer_oauth":true}}"#,
        )
        .unwrap();
        let opt_cfg = oc_config::ConfigService::load(oc_config::LoadOptions {
            directory: opt_dir.path().to_path_buf(),
            env: std::collections::HashMap::new(),
        })
        .unwrap();
        assert!(antigravity_legacy_oauth_enabled(&opt_cfg));
    }
}
