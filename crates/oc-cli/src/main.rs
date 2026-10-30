//! `opencode` binary: serve the HTTP/SSE API and the web app.

mod auth;
mod bootstrap;
mod cli;
mod lane;
mod net;
mod paths;
mod schedule;
mod scribe;
mod serve;
mod service;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::auth::service_get;
use crate::cli::{Cli, Command, ServeOptions, ServiceCommand};
use crate::serve::serve;

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
