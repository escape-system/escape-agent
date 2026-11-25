//! `opencode` binary: serve the HTTP/SSE API and the web app.

mod antigravity_cmd;
mod auth;
mod bootstrap;
mod cli;
mod lane;
mod net;
mod paths;
mod run;
mod schedule;
mod scribe;
mod serve;
mod service;

use clap::Parser;
use oc_alloc::MiMalloc;
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use crate::antigravity_cmd::run_auth;
use crate::auth::service_get;
use crate::cli::{ChatOptions, Cli, Command, RunOptions, ServeOptions, ServiceCommand};
use crate::run::{run_chat, run_once};
use crate::serve::serve;

fn main() -> anyhow::Result<()> {
    oc_alloc::configure_low_footprint();
    let workers = std::thread::available_parallelism().map_or(4, |count| count.get().min(8));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(32)
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default_level = match &cli.command {
        Some(Command::Serve { .. } | Command::Service { .. }) => "info",
        Some(Command::Run { .. } | Command::Chat { .. } | Command::Auth(_)) | None => "warn",
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .init();

    match cli.command {
        Some(Command::Run {
            prompt,
            model,
            agent,
            directory,
            session,
            db,
            json,
            ask_permissions,
        }) => {
            run_once(RunOptions {
                prompt,
                model,
                agent,
                directory,
                session,
                db,
                json,
                ask_permissions,
            })
            .await
        }
        Some(Command::Chat {
            model,
            agent,
            directory,
            session,
            db,
            ask_permissions,
        }) => {
            run_chat(ChatOptions {
                model,
                agent,
                directory,
                session,
                db,
                ask_permissions,
            })
            .await
        }
        None => {
            run_chat(ChatOptions {
                model: None,
                agent: None,
                directory: None,
                session: None,
                db: None,
                ask_permissions: false,
            })
            .await
        }
        Some(Command::Serve {
            hostname,
            port,
            password,
            directory,
            web_dist,
            cors,
            service,
            db,
        }) => {
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
        Some(Command::Service {
            command: ServiceCommand::Get { key },
        }) => service_get(&key),
        Some(Command::Auth(args)) => run_auth(args).await,
    }
}
