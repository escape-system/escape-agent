//! `opencode` binary: serve the HTTP/SSE API and the web app.

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
use tracing_subscriber::EnvFilter;

use crate::auth::service_get;
use crate::cli::{ChatOptions, Cli, Command, RunOptions, ServeOptions, ServiceCommand};
use crate::run::{run_chat, run_once};
use crate::serve::serve;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let default_level = match &cli.command {
        Some(Command::Serve { .. } | Command::Service { .. }) => "info",
        Some(Command::Run { .. } | Command::Chat { .. }) | None => "warn",
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
        }) => {
            run_once(RunOptions {
                prompt,
                model,
                agent,
                directory,
                session,
                db,
                json,
            })
            .await
        }
        Some(Command::Chat {
            model,
            agent,
            directory,
            session,
            db,
        }) => {
            run_chat(ChatOptions {
                model,
                agent,
                directory,
                session,
                db,
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
    }
}
