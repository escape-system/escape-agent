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
        /// Prompt interactively on stderr before approving `permission.asked` requests (default: auto-approve).
        #[arg(long)]
        ask_permissions: bool,
    },
    /// Start an interactive multi-turn terminal chat session (default when no subcommand is given).
    Chat {
        /// Prompt interactively on stderr before approving `permission.asked` requests (default: auto-approve).
        #[arg(long)]
        ask_permissions: bool,
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
    /// Authenticate providers and manage the Google Antigravity multi-account pool.
    Auth(AuthArgs),
    /// Expose deterministic Rust workspace tools over stdio MCP (`rmcp`) for human-driven IDE sessions.
    Mcp {
        /// Workspace root directory to serve (defaults to current working directory).
        #[arg(short = 'C', long)]
        directory: Option<PathBuf>,
        /// Expose read-only workspace inspection tools only (`read`, `glob`, `grep`, `code_map`).
        #[arg(long)]
        read_only: bool,
        /// Shell sandbox policy (`auto`, `bwrap`, `no-net`, `off`).
        #[arg(long, default_value = "auto")]
        sandbox: String,
    },
}

#[derive(Debug, clap::Args)]
pub(crate) struct AuthArgs {
    /// Working directory for project-local `.opencode/antigravity.json`.
    #[arg(short = 'C', long, global = true)]
    pub(crate) directory: Option<PathBuf>,
    /// SQLite database path (default: `<data>/escape.db` or `$OPENCODE_DB`).
    #[arg(long, env = "OPENCODE_DB", global = true)]
    pub(crate) db: Option<PathBuf>,
    /// Emit structured JSON output instead of formatted text.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: AuthCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AuthCommand {
    /// Sign in with Google Antigravity OAuth (multi-account) or store a provider API key.
    Login {
        /// Provider ID (`google`, `antigravity`, `openai`, `anthropic`, ...).
        #[arg(short, long, default_value = "google")]
        provider: String,
        /// Disable the localhost loopback callback server and enter the callback URL/code manually.
        #[arg(long)]
        no_browser: bool,
        /// Optional Google Cloud / Cloud Code Assist project ID override.
        #[arg(long)]
        project: Option<String>,
        /// Optional human label for this credential/account (e.g. `Work`, `Personal`).
        #[arg(short, long)]
        label: Option<String>,
        /// Optional authorization code or callback URL for manual completion.
        #[arg(long)]
        code: Option<String>,
        /// Explicit API key when authenticating a non-OAuth provider.
        #[arg(long, hide_env_values = true)]
        api_key: Option<String>,
        /// Explicitly opt into legacy consumer Antigravity OAuth (carries Article 6 `ToS` ban risk).
        #[arg(long)]
        allow_consumer_oauth: bool,
    },
    /// List stored credentials and Google Antigravity pool accounts.
    #[command(visible_alias = "ls")]
    List {
        /// Filter output by provider ID.
        #[arg(short, long)]
        provider: Option<String>,
    },
    /// Show Google Antigravity account pool status, active cursors, and cooldowns.
    Status {
        /// Refresh live quotas and upstream models before printing status.
        #[arg(long)]
        refresh: bool,
    },
    /// Show remaining quota across Claude and Gemini model groups.
    #[command(visible_alias = "quotas")]
    Quota {
        /// Read only locally cached quota snapshots instead of fetching upstream.
        #[arg(long)]
        cached: bool,
    },
    /// Set the active sticky Google Antigravity account by index, email, label, or token hash.
    Use { account: String },
    /// Re-enable a disabled or verification-flagged Google Antigravity account.
    Enable { account: String },
    /// Temporarily disable a Google Antigravity account from pool rotation.
    Disable { account: String },
    /// Remove a Google Antigravity account or stored `cred_` credential.
    #[command(visible_alias = "rm")]
    Remove { account: String },
    /// List or refresh Google Antigravity models (`google/antigravity-*`).
    Models {
        /// Query upstream Cloud Code Assist to discover models and update the local cache.
        #[arg(long)]
        refresh: bool,
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
    pub(crate) ask_permissions: bool,
}

pub(crate) struct ChatOptions {
    pub(crate) model: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) session: Option<String>,
    pub(crate) db: Option<PathBuf>,
    pub(crate) ask_permissions: bool,
}

pub(crate) struct McpOptions {
    pub(crate) directory: Option<PathBuf>,
    pub(crate) read_only: bool,
    pub(crate) sandbox: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_default_to_auto_approve_and_flag_opts_in() {
        let bare = Cli::try_parse_from(["opencode"]).unwrap();
        assert!(bare.command.is_none());

        let run_default = Cli::try_parse_from(["opencode", "run", "hi"]).unwrap();
        match run_default.command.unwrap() {
            Command::Run {
                ask_permissions, ..
            } => assert!(!ask_permissions),
            other => panic!("unexpected command: {other:?}"),
        }

        let run_ask = Cli::try_parse_from(["opencode", "run", "--ask-permissions", "hi"]).unwrap();
        match run_ask.command.unwrap() {
            Command::Run {
                ask_permissions, ..
            } => assert!(ask_permissions),
            other => panic!("unexpected command: {other:?}"),
        }

        let chat_default = Cli::try_parse_from(["opencode", "chat"]).unwrap();
        match chat_default.command.unwrap() {
            Command::Chat {
                ask_permissions, ..
            } => assert!(!ask_permissions),
            other => panic!("unexpected command: {other:?}"),
        }

        let chat_ask = Cli::try_parse_from(["opencode", "chat", "--ask-permissions"]).unwrap();
        match chat_ask.command.unwrap() {
            Command::Chat {
                ask_permissions, ..
            } => assert!(ask_permissions),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn auth_subcommands_parse_antigravity_flags() {
        let login = Cli::try_parse_from([
            "opencode",
            "auth",
            "login",
            "--no-browser",
            "--project",
            "proj-1",
            "--label",
            "Work",
        ])
        .unwrap();
        match login.command.unwrap() {
            Command::Auth(args) => match args.command {
                AuthCommand::Login {
                    provider,
                    no_browser,
                    project,
                    label,
                    ..
                } => {
                    assert_eq!(provider, "google");
                    assert!(no_browser);
                    assert_eq!(project.as_deref(), Some("proj-1"));
                    assert_eq!(label.as_deref(), Some("Work"));
                }
                other => panic!("unexpected auth subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }

        let quota =
            Cli::try_parse_from(["opencode", "auth", "--json", "quota", "--cached"]).unwrap();
        match quota.command.unwrap() {
            Command::Auth(args) => {
                assert!(args.json);
                assert!(matches!(args.command, AuthCommand::Quota { cached: true }));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let use_cmd = Cli::try_parse_from(["opencode", "auth", "use", "user@example.com"]).unwrap();
        match use_cmd.command.unwrap() {
            Command::Auth(args) => match args.command {
                AuthCommand::Use { account } => assert_eq!(account, "user@example.com"),
                other => panic!("unexpected auth subcommand: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn mcp_subcommand_parses_read_only_and_sandbox_flags() {
        let parsed = Cli::try_parse_from([
            "opencode",
            "mcp",
            "-C",
            "/tmp/ws",
            "--read-only",
            "--sandbox",
            "no-net",
        ])
        .unwrap();
        match parsed.command.unwrap() {
            Command::Mcp {
                directory,
                read_only,
                sandbox,
            } => {
                assert_eq!(directory, Some(PathBuf::from("/tmp/ws")));
                assert!(read_only);
                assert_eq!(sandbox, "no-net");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
