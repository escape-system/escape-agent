//! `opencode` binary: serve the HTTP/SSE API and the web app.

mod scribe;
mod service;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use clap::{Parser, Subcommand};
use oc_server::{AppState, router};
use oc_store::{
    Delivery, NewInbox, ScheduleAction, ScheduleOutcome, ScheduleWhen, Store, StoreError,
};
use oc_types::{
    EventEnvelope, EventLocation, EventPayload, InboxItem, MessageId, ServerInfo, ServerPaths,
    SessionId, SessionInboxEnqueued, now_millis,
};
use rand::Rng;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "opencode", version, about = "opencode-rust server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the HTTP/SSE server.
    Serve {
        /// Interface to bind.
        #[arg(long, default_value = "127.0.0.1")]
        hostname: String,
        /// Port to bind; 0 picks an ephemeral port.
        #[arg(long, default_value_t = 4096)]
        port: u16,
        /// Password for HTTP Basic auth. Omit to disable authentication.
        #[arg(long, env = "OPENCODE_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// Working directory served as the default location.
        #[arg(long)]
        directory: Option<PathBuf>,
        /// Directory containing the built web app (index.html, _assets).
        #[arg(long, env = "OC_WEB_DIST")]
        web_dist: Option<PathBuf>,
        /// Extra allowed CORS origin (repeatable).
        #[arg(long)]
        cors: Vec<String>,
        /// Register as a managed service and exit when the registration is lost.
        #[arg(long)]
        service: bool,
        /// SQLite database path (default: `<data>/opencode.db` or `$OPENCODE_DB`).
        #[arg(long, env = "OPENCODE_DB")]
        db: Option<PathBuf>,
    },
    /// Read the managed-service registration written by `serve --service`.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Print one registration field: `url`, `password`, `pid`, `id`, `version`.
    Get {
        #[arg(default_value = "url")]
        key: String,
    },
}

struct ServeOptions {
    hostname: String,
    port: u16,
    password: Option<String>,
    directory: Option<PathBuf>,
    web_dist: Option<PathBuf>,
    cors: Vec<String>,
    service_mode: bool,
    db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve {
            hostname,
            port,
            password,
            directory,
            web_dist,
            cors,
            service,
            db,
        } => {
            serve(ServeOptions {
                hostname,
                port,
                password,
                directory,
                web_dist,
                cors,
                service_mode: service,
                db,
            })
            .await
        }
        Command::Service {
            command: ServiceCommand::Get { key },
        } => service_get(&key),
    }
}

async fn serve(options: ServeOptions) -> anyhow::Result<()> {
    let ServeOptions {
        hostname,
        port,
        password,
        directory,
        web_dist,
        cors,
        service_mode,
        db,
    } = options;

    let directory = match directory {
        Some(path) => path,
        None => std::env::current_dir().context("resolve working directory")?,
    };
    // Keep `AppState.directory` absolute: location resolution joins it against
    // itself for requests without an explicit directory, and config discovery
    // should not depend on the process cwd changing later.
    let directory = std::path::absolute(&directory)
        .with_context(|| format!("resolve directory {}", directory.display()))?;

    let listener = bind_with_fallback(&hostname, port).await?;
    let local = listener.local_addr().context("read bound address")?;
    let url = format!("http://{hostname}:{}", local.port());

    // A managed service keeps the password it registered before, so a restart
    // does not invalidate the browser session/pairing links. The alias is the
    // canonical upstream env name; `OPENCODE_PASSWORD` keeps working through
    // clap. An explicit empty value disables auth (upstream `required()`).
    let channel = std::env::var("OPENCODE_CHANNEL").unwrap_or_else(|_| "latest".to_owned());
    let registration_path = service::registration_path(&channel);
    let registration = service::read(&registration_path).ok().flatten();
    let password = resolve_password(
        password,
        std::env::var("OPENCODE_SERVER_PASSWORD").ok(),
        service_mode,
        registration.as_ref(),
    );

    let info = ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        hostname: Some(hostname.clone()),
        urls: vec![url.clone()],
        paths: ServerPaths {
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
        },
        capabilities: None,
    };

    let services = open_services(&directory, db).await?;
    let state = AppState::new(info, password.clone(), directory.clone())
        .with_port(local.port())
        .with_cors_origins(cors)
        .with_store(services.store.clone())
        .with_config(services.config.clone())
        .with_catalog(services.catalog.clone());
    let mut state = state;
    if let Some(web_dist) = web_dist {
        state = state.with_web_dist(web_dist);
    }

    // Extensions own the MCP manager, the LSP supervisor, the plugin registry
    // and the lane service. They must exist before the runtime so the MCP/LSP
    // tool providers and the lane handle can be attached to it.
    let (state, extensions) = attach_extensions(state, &services, &directory);
    let (state, runtime, broker) = attach_runtime(state, &services, &extensions).await?;
    let pty = Arc::new(oc_pty::PtyManager::new(oc_pty::PtyConfig::default()));
    let state = state.with_pty(pty.clone());
    let lsp = Arc::clone(&extensions.lsp);

    let service_id = format!("{}-{:x}", std::process::id(), oc_types::now_millis());
    if service_mode {
        let registration = service::ServiceRegistration {
            id: service_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            url: url.clone(),
            pid: std::process::id(),
            password: password.clone(),
        };
        service::write(&registration_path, &registration)
            .with_context(|| format!("write {}", registration_path.display()))?;
        println!("service registration: {}", registration_path.display());
    }

    println!("opencode server listening on {url}");
    if password.is_none() {
        println!("authentication disabled (loopback only)");
    }

    let shutdown = CancellationToken::new();
    let schedule_runner = start_background_tasks(
        &state,
        &shutdown,
        service_mode.then_some((registration_path.as_path(), service_id.as_str())),
    );

    let result = axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await
        .context("serve");
    shutdown_services(schedule_runner, &lsp, &extensions, &runtime, &pty, &broker).await;
    if service_mode {
        service::remove_if_owned(&registration_path, &service_id);
    }
    result
}

/// Stop everything spawned for the server, in dependency order.
async fn shutdown_services(
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

struct Services {
    store: std::sync::Arc<oc_store::Store>,
    config: std::sync::Arc<oc_config::ConfigService>,
    catalog: std::sync::Arc<oc_llm::Catalog>,
}

async fn open_services(
    directory: &std::path::Path,
    db: Option<PathBuf>,
) -> anyhow::Result<Services> {
    let db_path = db.unwrap_or_else(default_db_path);
    let store = std::sync::Arc::new(
        oc_store::Store::open(&db_path)
            .await
            .with_context(|| format!("open database {}", db_path.display()))?,
    );
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

struct Extensions {
    lsp: Arc<oc_lsp::LspSupervisor>,
    lanes: Arc<oc_lanes::LaneService>,
    providers: Vec<Arc<dyn oc_runtime::ToolProvider>>,
}

async fn attach_runtime(
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

fn attach_extensions(
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
fn start_background_tasks(
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

/// How often the runner looks for due schedules.
const SCHEDULE_TICK: Duration = Duration::from_secs(1);
/// How many due schedules one tick admits.
const SCHEDULE_BATCH: usize = 16;
/// A firing this far past its due time is recorded as late.
const SCHEDULE_LATE_AFTER_MS: i64 = 60_000;

/// Start the schedule runner once the runtime owns the store.
///
/// Returns `None` when the server was built without a runtime or store: there
/// is then nothing to admit prompts into, so no task is spawned.
fn spawn_schedule_runner(state: AppState, shutdown: CancellationToken) -> Option<JoinHandle<()>> {
    let store = state.store()?.clone();
    state.runtime()?;
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SCHEDULE_TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let now = now_millis();
            let due = match store.due_schedules(now, SCHEDULE_BATCH).await {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error, "schedule poll failed");
                    continue;
                }
            };
            for schedule in due {
                if shutdown.is_cancelled() {
                    break;
                }
                if let Err(error) = fire_schedule(&state, &schedule, now).await {
                    tracing::warn!(%error, "schedule failed to fire");
                }
            }
        }
    }))
}

/// Admit one due schedule's prompt, record the firing, then wake its session.
///
/// The inbox id is derived from the schedule id and run number so a crash
/// between admission and the firing record replays as a conflict instead of a
/// duplicate prompt. The firing is recorded before `notify`, so the next tick
/// cannot fire the same due time again.
#[allow(clippy::too_many_lines)] // one linear admit-record-wake sequence
async fn fire_schedule(state: &AppState, schedule: &Value, now: i64) -> anyhow::Result<()> {
    let store = state.store().context("schedule runner has no store")?;
    let Some(id) = schedule.get("id").and_then(Value::as_str) else {
        anyhow::bail!("due schedule has no id");
    };
    let due = schedule
        .get("nextDueAt")
        .and_then(Value::as_i64)
        .unwrap_or(now);
    let run_count = schedule
        .get("runCount")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    let when: ScheduleWhen =
        match serde_json::from_value(schedule.get("when").cloned().unwrap_or(Value::Null)) {
            Ok(when) => when,
            Err(error) => {
                fail_schedule(
                    store,
                    id,
                    None,
                    run_count,
                    &format!("invalid when: {error}"),
                )
                .await;
                return Ok(());
            }
        };
    let next = match when.next_after(now, due) {
        Ok(next) => next,
        Err(error) => {
            fail_schedule(
                store,
                id,
                None,
                run_count,
                &format!("invalid when: {error}"),
            )
            .await;
            return Ok(());
        }
    };

    let Some(session_id) = schedule.get("sessionID").and_then(Value::as_str) else {
        fail_schedule(store, id, next, run_count, "schedule has no session").await;
        return Ok(());
    };
    let Some(session) = store.get_session(session_id).await? else {
        fail_schedule(store, id, next, run_count, "session no longer exists").await;
        return Ok(());
    };
    if session_archived(&session) {
        fail_schedule(store, id, next, run_count, "session is archived").await;
        return Ok(());
    }
    let prompt = match action_prompt(schedule.get("action")) {
        Ok(prompt) => prompt,
        Err(error) => {
            fail_schedule(store, id, next, run_count, &error).await;
            return Ok(());
        }
    };

    let outcome = schedule_outcome(due, now);
    let message_id = format!("msg_{id}_{run_count}");
    let payload = json!({ "text": prompt, "time": { "created": now } });

    // The prompt is admitted as a `queue` inbox row only. The runtime promotes
    // it when a turn settles, so a message must not be written here: an early
    // row would be visible to (and run alongside) a turn already in flight.
    match store
        .enqueue(NewInbox {
            id: message_id.clone(),
            session_id: session_id.to_owned(),
            kind: "user".to_owned(),
            payload,
            delivery: Delivery::Queue,
        })
        .await
    {
        Ok(item) => {
            let label = outcome.as_str();
            if let Err(error) = publish_inbox_enqueued(state, session_id, &message_id, &item).await
            {
                tracing::warn!(%error, "could not publish schedule inbox event");
            }
            publish_schedule_fired(state, &session, session_id, id, label);
        }
        // An earlier tick admitted this run but did not record it; finish the
        // record and wake the session instead of adding a second prompt.
        Err(StoreError::Conflict(_)) => {
            tracing::debug!(schedule = %id, "scheduled prompt already admitted");
        }
        Err(error) => return Err(error.into()),
    }

    match store
        .mark_schedule_fired(id, outcome, next, run_count)
        .await
        .with_context(|| format!("record schedule {id}"))
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            tracing::warn!(
                schedule = %id,
                "schedule vanished before its firing was recorded"
            );
        }
        Err(error) => return Err(error),
    }
    if let Some(runtime) = state.runtime()
        && let Err(error) = runtime.notify(session_id).await
    {
        tracing::warn!(session = %session_id, %error, "could not wake scheduled session");
    }
    Ok(())
}

/// Record a failed firing so the schedule advances (or finishes) without
/// retrying the same due time on every tick.
async fn fail_schedule(store: &Store, id: &str, next: Option<i64>, run_count: i64, reason: &str) {
    tracing::warn!(schedule = %id, %reason, "schedule could not fire");
    if let Err(error) = store
        .mark_schedule_fired(id, ScheduleOutcome::Failed, next, run_count)
        .await
    {
        tracing::warn!(schedule = %id, %error, "could not record failed schedule");
    }
}

/// The prompt text of a stored prompt action.
fn action_prompt(action: Option<&Value>) -> Result<String, String> {
    let action = action.ok_or_else(|| "schedule action is missing".to_owned())?;
    let action: ScheduleAction = serde_json::from_value(action.clone())
        .map_err(|error| format!("invalid schedule action: {error}"))?;
    match action {
        ScheduleAction::Prompt { prompt } if prompt.is_empty() => {
            Err("schedule prompt is empty".to_owned())
        }
        ScheduleAction::Prompt { prompt } => Ok(prompt),
    }
}

fn schedule_outcome(due: i64, now: i64) -> ScheduleOutcome {
    if now - due >= SCHEDULE_LATE_AFTER_MS {
        ScheduleOutcome::Late
    } else {
        ScheduleOutcome::Fired
    }
}

fn session_archived(session: &Value) -> bool {
    session
        .pointer("/time/archived")
        .is_some_and(|value| !value.is_null())
}

/// Persist the `session.inbox.enqueued` event for an admitted prompt.
async fn publish_inbox_enqueued(
    state: &AppState,
    session_id: &str,
    inbox_id: &str,
    item: &Value,
) -> anyhow::Result<()> {
    let item: InboxItem =
        serde_json::from_value(item.clone()).context("inbox row is not a wire item")?;
    let payload = SessionInboxEnqueued {
        session_id: SessionId::from_wire(session_id),
        inbox_id: MessageId::from_wire(inbox_id),
        item,
    };
    oc_server::data::events::publish_durable(
        state,
        session_id,
        SessionInboxEnqueued::TYPE,
        payload.data(),
        SessionInboxEnqueued::VERSION,
    )
    .await
    .map_err(|error| anyhow::anyhow!("publish inbox event: {}", error.0.body()))?;
    Ok(())
}

/// Publish the ephemeral `schedule.fired` event for a schedule that fired.
fn publish_schedule_fired(
    state: &AppState,
    session: &Value,
    session_id: &str,
    id: &str,
    outcome: &'static str,
) {
    let mut event = EventEnvelope::now(
        "schedule.fired",
        json!({
            "scheduleID": id,
            "sessionID": session_id,
            "outcome": outcome,
        }),
    );
    event.location = session
        .pointer("/location/directory")
        .and_then(Value::as_str)
        .map(|directory| EventLocation {
            directory: directory.to_owned(),
        });
    state.publish(event);
}

/// Lane concurrency from the merged `lanes` config object, or the defaults.
///
/// The documented wire shape is a flat map (`{"cargo": 1, "default": 8}`),
/// where `default` and `max_queue` are reserved keys. A serialized
/// [`oc_lanes::LaneConfig`] is accepted too; it is unambiguous because the
/// typed shape always carries a `lanes` object.
fn lane_config(config: &oc_config::ConfigService) -> oc_lanes::LaneConfig {
    lane_config_from_value(config.merged().get("lanes"))
}

fn lane_config_from_value(value: Option<&serde_json::Value>) -> oc_lanes::LaneConfig {
    let Some(value) = value.and_then(serde_json::Value::as_object) else {
        return oc_lanes::LaneConfig::default();
    };
    // `max_queue` also exists in the flat shape, so only the nested `lanes`
    // object can identify a serialized `LaneConfig`.
    if value.get("lanes").is_some_and(serde_json::Value::is_object) {
        return serde_json::from_value(serde_json::Value::Object(value.clone())).unwrap_or_else(
            |error| {
                tracing::warn!(%error, "invalid `lanes` configuration; using defaults");
                oc_lanes::LaneConfig::default()
            },
        );
    }
    let mut lanes = oc_lanes::LaneConfig::default();
    for (lane, value) in value {
        let Some(concurrency) = value.as_u64().and_then(|n| usize::try_from(n).ok()) else {
            tracing::warn!(%lane, "ignoring non-numeric lane concurrency");
            continue;
        };
        match lane.as_str() {
            "default" => lanes.default_concurrency = concurrency,
            "max_queue" => lanes.max_queue = concurrency,
            _ => {
                lanes.lanes.insert(lane.clone(), concurrency);
            }
        }
    }
    lanes
}

/// Publishes lane lifecycle changes as ephemeral SSE events.
#[derive(Clone)]
struct BusLaneEvents {
    bus: tokio::sync::broadcast::Sender<Arc<EventEnvelope>>,
    directory: String,
}

impl oc_lanes::LaneEvents for BusLaneEvents {
    fn changed(&self, ticket: &oc_lanes::Ticket) {
        let mut data = serde_json::json!({
            "lane": &ticket.lane,
            "ticket": ticket,
        });
        if let Some(session) = &ticket.owner_session {
            data["sessionID"] = serde_json::Value::String(session.clone());
        }
        let mut event = EventEnvelope::now(lane_event_kind(&ticket.state), data);
        event.location = Some(EventLocation {
            directory: self.directory.clone(),
        });
        // A send error only means no subscriber is connected.
        let _ = self.bus.send(Arc::new(event));
    }
}

fn lane_event_kind(state: &oc_lanes::TicketState) -> &'static str {
    match state {
        oc_lanes::TicketState::Queued => "lane.queued",
        oc_lanes::TicketState::Running => "lane.started",
        oc_lanes::TicketState::Succeeded { .. } | oc_lanes::TicketState::Failed { .. } => {
            "lane.finished"
        }
        oc_lanes::TicketState::Cancelled => "lane.cancelled",
    }
}

fn default_db_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        return PathBuf::from(dir).join("opencode.db");
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("opencode").join("opencode.db");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/opencode/opencode.db");
    }
    std::env::temp_dir().join("opencode").join("opencode.db")
}

fn resolve_password(
    explicit: Option<String>,
    env_alias: Option<String>,
    service_mode: bool,
    registration: Option<&service::ServiceRegistration>,
) -> Option<String> {
    match explicit {
        Some(password) if !password.is_empty() => return Some(password),
        // `--password ""` is an explicit "no auth", not "look elsewhere".
        Some(_) => return None,
        None => {}
    }
    if let Some(password) = env_alias.filter(|password| !password.is_empty()) {
        return Some(password);
    }
    if service_mode {
        if let Some(password) = registration
            .and_then(|registration| registration.password.clone())
            .filter(|password| !password.is_empty())
        {
            return Some(password);
        }
        return Some(random_password());
    }
    None
}

/// `opencode service get <key>`: read the managed-service registration.
fn service_get(key: &str) -> anyhow::Result<()> {
    let channel = std::env::var("OPENCODE_CHANNEL").unwrap_or_else(|_| "latest".to_owned());
    let path = service::registration_path(&channel);
    let registration = service::read(&path)
        .with_context(|| format!("read {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("no service registration at {}", path.display()))?;
    match key {
        "url" => println!("{}", registration.url),
        "password" => println!("{}", registration.password.unwrap_or_default()),
        "pid" => println!("{}", registration.pid),
        "id" => println!("{}", registration.id),
        "version" => println!("{}", registration.version),
        other => anyhow::bail!(
            "unknown service key {other:?}; expected url, password, pid, id or version"
        ),
    }
    Ok(())
}

fn random_password() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

async fn bind_with_fallback(hostname: &str, port: u16) -> io::Result<TcpListener> {
    match TcpListener::bind((hostname, port)).await {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse && port != 0 => {
            for candidate in port.saturating_add(1)..=u16::MAX {
                match TcpListener::bind((hostname, candidate)).await {
                    Ok(listener) => return Ok(listener),
                    Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        action_prompt, lane_config_from_value, lane_event_kind, resolve_password, schedule_outcome,
        service::ServiceRegistration, session_archived,
    };
    use oc_lanes::{LaneConfig, TicketState};
    use oc_store::ScheduleOutcome;
    use serde_json::json;

    fn registration(password: Option<&str>) -> ServiceRegistration {
        ServiceRegistration {
            id: "svc-test".to_owned(),
            version: "0.1.0".to_owned(),
            url: "http://127.0.0.1:4096".to_owned(),
            pid: 42,
            password: password.map(str::to_owned),
        }
    }

    #[test]
    fn explicit_password_wins_and_empty_disables_auth() {
        assert_eq!(
            resolve_password(Some("cli".to_owned()), Some("env".to_owned()), false, None)
                .as_deref(),
            Some("cli")
        );
        assert_eq!(
            resolve_password(Some(String::new()), Some("env".to_owned()), true, None),
            None,
            "an explicit empty password must not fall through to env or a random service password"
        );
    }

    #[test]
    fn server_password_alias_and_service_registration_are_fallbacks() {
        assert_eq!(
            resolve_password(None, Some("env".to_owned()), false, None).as_deref(),
            Some("env")
        );
        let stored = registration(Some("svc-pass"));
        assert_eq!(
            resolve_password(None, None, true, Some(&stored)).as_deref(),
            Some("svc-pass"),
            "a service restart must reuse the registered password"
        );
        assert!(
            resolve_password(
                None,
                Some(String::new()),
                true,
                Some(&registration(Some("")))
            )
            .is_some(),
            "an empty alias/stored password is not a credential; service mode generates one"
        );
        assert!(
            resolve_password(None, None, true, Some(&registration(None))).is_some(),
            "service mode without a stored password generates one"
        );
        assert_eq!(resolve_password(None, None, false, None), None);
    }

    #[test]
    fn action_prompt_decodes_stored_prompt_actions() {
        let stored = json!({ "type": "prompt", "prompt": "Run the nightly audit" });
        assert_eq!(
            action_prompt(Some(&stored)).expect("prompt action"),
            "Run the nightly audit"
        );
    }

    #[test]
    fn action_prompt_rejects_missing_and_foreign_actions() {
        assert!(action_prompt(None).is_err());
        assert!(action_prompt(Some(&json!({ "type": "prompt" }))).is_err());
        assert!(action_prompt(Some(&json!({ "type": "prompt", "prompt": "" }))).is_err());
        assert!(action_prompt(Some(&json!({ "type": "command", "prompt": "hi" }))).is_err());
    }

    #[test]
    fn schedule_outcome_is_late_after_a_minute() {
        assert!(matches!(
            schedule_outcome(1_000, 1_000),
            ScheduleOutcome::Fired
        ));
        assert!(matches!(
            schedule_outcome(1_000, 60_999),
            ScheduleOutcome::Fired
        ));
        assert!(matches!(
            schedule_outcome(1_000, 61_000),
            ScheduleOutcome::Late
        ));
    }

    #[test]
    fn session_archived_reads_the_archived_timestamp() {
        assert!(!session_archived(&json!({ "time": { "created": 1 } })));
        assert!(!session_archived(&json!({ "time": { "archived": null } })));
        assert!(session_archived(&json!({ "time": { "archived": 5 } })));
    }

    #[test]
    fn lane_events_cover_every_state() {
        assert_eq!(lane_event_kind(&TicketState::Queued), "lane.queued");
        assert_eq!(lane_event_kind(&TicketState::Running), "lane.started");
        assert_eq!(
            lane_event_kind(&TicketState::Succeeded { exit: 0 }),
            "lane.finished"
        );
        assert_eq!(
            lane_event_kind(&TicketState::Failed {
                exit: 1,
                message: "failed".to_owned(),
            }),
            "lane.finished"
        );
        assert_eq!(lane_event_kind(&TicketState::Cancelled), "lane.cancelled");
    }

    #[test]
    fn lane_config_defaults_when_absent() {
        assert_eq!(lane_config_from_value(None), LaneConfig::default());
        assert_eq!(
            lane_config_from_value(Some(&json!("invalid"))),
            LaneConfig::default()
        );
    }

    #[test]
    fn lane_config_reads_the_documented_flat_shape() {
        let config = lane_config_from_value(Some(&json!({
            "cargo": 2,
            "gpu": 1,
            "default": 3,
            "max_queue": 9,
        })));
        assert_eq!(config.concurrency_for("cargo:workspace"), 1);
        assert_eq!(config.lanes.get("cargo"), Some(&2));
        assert_eq!(config.lanes.get("gpu"), Some(&1));
        assert_eq!(config.default_concurrency, 3);
        assert_eq!(config.max_queue, 9);
    }

    #[test]
    fn lane_config_flat_shape_keeps_max_queue_flat() {
        let config = lane_config_from_value(Some(&json!({
            "cargo": 2,
            "network": 4,
            "max_queue": 11,
        })));
        assert_eq!(config.lanes.get("cargo"), Some(&2));
        assert_eq!(config.lanes.get("network"), Some(&4));
        assert_eq!(config.max_queue, 11);
        assert_eq!(
            config.default_concurrency,
            LaneConfig::default().default_concurrency
        );
    }

    #[test]
    fn lane_config_typed_empty_lanes_is_not_flat() {
        let config = lane_config_from_value(Some(&json!({
            "lanes": {},
            "default_concurrency": 6,
            "max_queue": 12,
        })));
        assert!(config.lanes.is_empty());
        assert_eq!(config.default_concurrency, 6);
        assert_eq!(config.max_queue, 12);
    }

    #[test]
    fn lane_config_reads_the_typed_shape() {
        let config = lane_config_from_value(Some(&json!({
            "lanes": { "gpu": 2 },
            "default_concurrency": 5,
            "max_queue": 7,
        })));
        assert_eq!(config.lanes.get("gpu"), Some(&2));
        assert_eq!(config.default_concurrency, 5);
        assert_eq!(config.max_queue, 7);
    }
}
