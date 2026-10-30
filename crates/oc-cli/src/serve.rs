//! `opencode serve`: bind the listener, wire services and shut down gracefully.

use std::sync::Arc;

use anyhow::Context;
use oc_server::{AppState, router};
use oc_types::{ServerInfo, ServerPaths};
use tokio_util::sync::CancellationToken;

use crate::auth::resolve_password;
use crate::bootstrap::{
    attach_extensions, attach_runtime, open_services, shutdown_services, start_background_tasks,
};
use crate::cli::ServeOptions;
use crate::net::bind_with_fallback;
use crate::service;

pub(crate) async fn serve(options: ServeOptions) -> anyhow::Result<()> {
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
