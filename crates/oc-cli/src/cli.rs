//! Clap definitions for the `opencode` command line.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "opencode",
    version,
    about = "opencode-rust AI coding agent & server"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Execute a single prompt in the terminal and exit when the turn settles.
    Run {
        /// Prompt text to send to the agent (reads from stdin if omitted and piped).
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
        /// Model reference (`provider/id[#variant]`).
        #[arg(short, long)]
        model: Option<String>,
        /// Agent identifier (`build`, `plan`, etc.).
        #[arg(short, long)]
        agent: Option<String>,
        /// Working directory for the session.
        #[arg(short = 'C', long)]
        directory: Option<PathBuf>,
        /// Resume or target a specific session ID.
        #[arg(short, long)]
        session: Option<String>,
        /// SQLite database path (default: `<data>/escape.db` or `$OPENCODE_DB`).
        #[arg(long, env = "OPENCODE_DB")]
        db: Option<PathBuf>,
        /// Emit raw JSONL event envelopes to stdout instead of formatted text.
        #[arg(long)]
        json: bool,
    },
    /// Start an interactive multi-turn terminal chat session (default when no subcommand is given).
    Chat {
        /// Model reference (`provider/id[#variant]`).
        #[arg(short, long)]
        model: Option<String>,
        /// Agent identifier (`build`, `plan`, etc.).
        #[arg(short, long)]
        agent: Option<String>,
        /// Working directory for the session.
        #[arg(short = 'C', long)]
        directory: Option<PathBuf>,
        /// Resume or target a specific session ID.
        #[arg(short, long)]
        session: Option<String>,
        /// SQLite database path (default: `<data>/escape.db` or `$OPENCODE_DB`).
        #[arg(long, env = "OPENCODE_DB")]
        db: Option<PathBuf>,
    },
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
        /// SQLite database path (default: `<data>/escape.db` or `$OPENCODE_DB`).
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
pub(crate) enum ServiceCommand {
    /// Print one registration field: `url`, `password`, `pid`, `id`, `version`.
    Get {
        #[arg(default_value = "url")]
        key: String,
    },
}

pub(crate) struct ServeOptions {
    pub(crate) hostname: String,
    pub(crate) port: u16,
    pub(crate) password: Option<String>,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) web_dist: Option<PathBuf>,
    pub(crate) cors: Vec<String>,
    pub(crate) service_mode: bool,
    pub(crate) db: Option<PathBuf>,
}

pub(crate) struct RunOptions {
    pub(crate) prompt: Vec<String>,
    pub(crate) model: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) session: Option<String>,
    pub(crate) db: Option<PathBuf>,
    pub(crate) json: bool,
}

pub(crate) struct ChatOptions {
    pub(crate) model: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) session: Option<String>,
    pub(crate) db: Option<PathBuf>,
}
