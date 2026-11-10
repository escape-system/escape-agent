//! `opencode run` and `opencode chat`: terminal execution and interactive REPL.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use oc_server::AppState;
use oc_store::{Delivery, NewInbox, NewSession, SessionPatch};
use oc_types::permission::PermissionReply;
use oc_types::{
    EventEnvelope, EventPayload, InboxItem, MessageId, ModelRef, ServerInfo, ServerPaths,
    SessionId, SessionInboxEnqueued, now_millis, project_id_from_directory,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

use crate::bootstrap::{attach_extensions, attach_runtime, open_services};
use crate::cli::{ChatOptions, RunOptions};

/// Execute a single prompt in the terminal and exit when the turn settles.
pub(crate) async fn run_once(options: RunOptions) -> anyhow::Result<()> {
    let RunOptions {
        prompt,
        model,
        agent,
        directory,
        session,
        db,
        json: json_mode,
    } = options;

    let text = resolve_prompt_text(&prompt)?;
    let directory = resolve_directory(directory)?;
    let host = boot_terminal_host(&directory, db).await?;

    let model_ref = match model.as_deref() {
        Some(raw) => Some(parse_and_validate_model(&host.services.catalog, raw)?),
        None => None,
    };
    if let Some(agent_id) = agent.as_deref() {
        validate_agent(&host.services.config, agent_id)?;
    }

    let session_id = ensure_session(
        &host,
        &directory,
        session.as_deref(),
        agent.as_deref(),
        model_ref.as_ref(),
    )
    .await?;

    let mut rx = host.state.bus().subscribe();
    enqueue_user_prompt(&host.state, &session_id, &text).await?;

    let outcome = stream_turn(&host, &session_id, &mut rx, json_mode).await;
    host.shutdown().await;
    outcome
}

/// Start an interactive multi-turn terminal chat session.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_chat(options: ChatOptions) -> anyhow::Result<()> {
    let ChatOptions {
        model,
        agent,
        directory,
        session,
        db,
    } = options;

    let directory = resolve_directory(directory)?;
    let host = boot_terminal_host(&directory, db).await?;

    let model_ref = match model.as_deref() {
        Some(raw) => Some(parse_and_validate_model(&host.services.catalog, raw)?),
        None => None,
    };
    if let Some(agent_id) = agent.as_deref() {
        validate_agent(&host.services.config, agent_id)?;
    }

    let mut session_id = ensure_session(
        &host,
        &directory,
        session.as_deref(),
        agent.as_deref(),
        model_ref.as_ref(),
    )
    .await?;

    eprintln!(
        "\x1b[1;36mopencode\x1b[0m v{} • session \x1b[2m{}\x1b[0m • dir \x1b[2m{}\x1b[0m",
        env!("CARGO_PKG_VERSION"),
        session_id,
        directory.display()
    );
    eprintln!(
        "\x1b[2mCommands: /new, /compact, /model <provider/id>, /agent <id>, /status, /help, /exit\x1b[0m\n"
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    loop {
        eprint!("\x1b[1;32mopencode>\x1b[0m ");
        let _ = std::io::stderr().flush();

        let line = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!();
                break;
            }
            next = reader.next_line() => match next {
                Ok(Some(line)) => line,
                Ok(None) => {
                    eprintln!();
                    break;
                }
                Err(error) => {
                    eprintln!("\x1b[31mread error: {error}\x1b[0m");
                    break;
                }
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if matches!(trimmed, "/exit" | "/quit" | ":q") {
            break;
        }
        if trimmed == "/help" {
            print_chat_help();
            continue;
        }
        if trimmed == "/new" {
            session_id = ensure_session(
                &host,
                &directory,
                None,
                agent.as_deref(),
                model_ref.as_ref(),
            )
            .await?;
            eprintln!("\x1b[2m[new session: {session_id}]\x1b[0m");
            continue;
        }
        if trimmed == "/status" {
            print_session_status(&host, &session_id).await?;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/model") {
            let arg = rest.trim();
            if arg.is_empty() {
                print_session_status(&host, &session_id).await?;
                continue;
            }
            match parse_and_validate_model(&host.services.catalog, arg) {
                Ok(parsed) => {
                    host.services
                        .store
                        .patch_session(
                            &session_id,
                            SessionPatch {
                                model: Some(json!(&parsed)),
                                ..SessionPatch::default()
                            },
                        )
                        .await?;
                    eprintln!(
                        "\x1b[2m[model set to {}/{}]\x1b[0m",
                        parsed.provider_id, parsed.id
                    );
                }
                Err(error) => eprintln!("\x1b[31m{error}\x1b[0m"),
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/agent") {
            let arg = rest.trim();
            if arg.is_empty() {
                print_session_status(&host, &session_id).await?;
                continue;
            }
            match validate_agent(&host.services.config, arg) {
                Ok(()) => {
                    host.services
                        .store
                        .patch_session(
                            &session_id,
                            SessionPatch {
                                agent: Some(arg.to_owned()),
                                ..SessionPatch::default()
                            },
                        )
                        .await?;
                    eprintln!("\x1b[2m[agent set to {arg}]\x1b[0m");
                }
                Err(error) => eprintln!("\x1b[31m{error}\x1b[0m"),
            }
            continue;
        }
        if matches!(trimmed, "/compact" | "/compress") {
            let mut rx = host.state.bus().subscribe();
            enqueue_compaction(&host.state, &session_id).await?;
            if let Err(error) = stream_turn(&host, &session_id, &mut rx, false).await {
                eprintln!("\x1b[31m{error}\x1b[0m");
            } else {
                eprintln!("\x1b[2m[session compacted]\x1b[0m");
            }
            continue;
        }

        let mut rx = host.state.bus().subscribe();
        enqueue_user_prompt(&host.state, &session_id, trimmed).await?;
        if let Err(error) = stream_turn(&host, &session_id, &mut rx, false).await {
            eprintln!("\x1b[31m{error}\x1b[0m");
        }
        println!();
    }

    host.shutdown().await;
    Ok(())
}

struct TerminalHost {
    services: crate::bootstrap::Services,
    extensions: crate::bootstrap::Extensions,
    state: AppState,
    runtime: std::sync::Arc<oc_runtime::Runtime>,
    broker: std::sync::Arc<oc_server::PermissionBroker>,
}

impl TerminalHost {
    async fn shutdown(self) {
        self.extensions.lsp.shutdown_all().await;
        self.extensions.lanes.shutdown().await;
        self.runtime.shutdown().await;
        self.broker.reject_all("The CLI session is shutting down.");
    }
}

async fn boot_terminal_host(directory: &Path, db: Option<PathBuf>) -> anyhow::Result<TerminalHost> {
    let info = ServerInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        hostname: Some("127.0.0.1".to_owned()),
        urls: Vec::new(),
        paths: ServerPaths {
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
        },
        capabilities: None,
    };
    let services = open_services(directory, db).await?;
    let state = AppState::new(info, None, directory.to_path_buf())
        .with_store(services.store.clone())
        .with_config(services.config.clone())
        .with_catalog(services.catalog.clone());
    let (state, extensions) = attach_extensions(state, &services, directory).await?;
    let (state, runtime, broker) = attach_runtime(state, &services, &extensions).await?;
    Ok(TerminalHost {
        services,
        extensions,
        state,
        runtime,
        broker,
    })
}

fn resolve_directory(directory: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let dir = match directory {
        Some(path) => path,
        None => std::env::current_dir().context("resolve working directory")?,
    };
    std::path::absolute(&dir).with_context(|| format!("resolve directory {}", dir.display()))
}

fn resolve_prompt_text(parts: &[String]) -> anyhow::Result<String> {
    let joined = parts.join(" ");
    if !joined.trim().is_empty() {
        return Ok(joined);
    }
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("read prompt from stdin")?;
        if !buf.trim().is_empty() {
            return Ok(buf);
        }
    }
    bail!("Prompt must not be empty. Usage: `opencode run \"<prompt>\"`")
}

fn parse_and_validate_model(
    catalog: &oc_llm::Catalog,
    reference: &str,
) -> anyhow::Result<ModelRef> {
    let parsed = oc_llm::parse_reference(reference)
        .with_context(|| format!("invalid model reference `{reference}` (expected provider/id)"))?;
    catalog
        .resolve(reference)
        .with_context(|| format!("resolve model `{reference}`"))?;
    Ok(ModelRef {
        id: parsed.id,
        provider_id: parsed.provider,
        variant: parsed.variant,
    })
}

fn validate_agent(config: &oc_config::ConfigService, agent_id: &str) -> anyhow::Result<()> {
    if config.agent(agent_id).is_none() {
        bail!("Agent not found: `{agent_id}`");
    }
    Ok(())
}

async fn ensure_session(
    host: &TerminalHost,
    directory: &Path,
    existing_id: Option<&str>,
    agent: Option<&str>,
    model: Option<&ModelRef>,
) -> anyhow::Result<String> {
    if let Some(id) = existing_id
        && host.services.store.get_session(id).await?.is_some()
    {
        if agent.is_some() || model.is_some() {
            host.services
                .store
                .patch_session(
                    id,
                    SessionPatch {
                        agent: agent.map(str::to_owned),
                        model: model.map(|m| json!(m)),
                        ..SessionPatch::default()
                    },
                )
                .await?;
        }
        return Ok(id.to_owned());
    }
    let id = existing_id.map_or_else(|| SessionId::new().into_inner(), str::to_owned);
    let dir_str = directory.to_string_lossy().into_owned();
    let canonical = std::fs::canonicalize(directory)
        .map_or_else(|_| dir_str.clone(), |p| p.to_string_lossy().into_owned());
    let project_id = project_id_from_directory(&canonical);
    host.services
        .store
        .create_session(NewSession {
            id: id.clone(),
            parent_id: None,
            project_id: project_id.as_str().to_owned(),
            directory: dir_str,
            title: None,
            agent: agent.map(str::to_owned),
            model: model.map(|m| json!(m)),
            metadata: None,
            permissions: None,
            time_created: now_millis(),
        })
        .await
        .context("create session")?;
    Ok(id)
}

async fn enqueue_user_prompt(
    state: &AppState,
    session_id: &str,
    text: &str,
) -> anyhow::Result<String> {
    let store = state.store().context("store not attached")?;
    let runtime = state.runtime().context("runtime not attached")?;
    let id = MessageId::new().into_inner();
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: "user".to_owned(),
            payload: json!({ "text": text }),
            delivery: Delivery::Queue,
        })
        .await
        .context("enqueue user prompt")?;
    publish_inbox(state, session_id, &id, &item).await?;
    runtime.notify(session_id).await.context("notify runtime")?;
    Ok(id)
}

async fn enqueue_compaction(state: &AppState, session_id: &str) -> anyhow::Result<String> {
    let store = state.store().context("store not attached")?;
    let runtime = state.runtime().context("runtime not attached")?;
    let id = MessageId::new().into_inner();
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: "compaction".to_owned(),
            payload: json!({}),
            delivery: Delivery::Queue,
        })
        .await
        .context("enqueue compaction")?;
    publish_inbox(state, session_id, &id, &item).await?;
    runtime.notify(session_id).await.context("notify runtime")?;
    Ok(id)
}

async fn publish_inbox(
    state: &AppState,
    session_id: &str,
    inbox_id: &str,
    item: &Value,
) -> anyhow::Result<()> {
    let item: InboxItem =
        serde_json::from_value(item.clone()).context("decode enqueued inbox item")?;
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

#[allow(clippy::too_many_lines)]
async fn stream_turn(
    host: &TerminalHost,
    session_id: &str,
    rx: &mut broadcast::Receiver<std::sync::Arc<EventEnvelope>>,
    json_mode: bool,
) -> anyhow::Result<()> {
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut in_reasoning = false;
    let mut printed_text = false;

    loop {
        let envelope = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = host.runtime.interrupt(session_id).await;
                eprintln!("\n\x1b[33m[interrupted]\x1b[0m");
                bail!("execution interrupted by user");
            }
            recv = rx.recv() => match recv {
                Ok(envelope) => envelope,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        };

        if envelope.kind == "permission.asked" {
            let ask_session = envelope
                .data
                .get("sessionID")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let request_id = envelope
                .data
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let action = envelope
                .data
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            if !ask_session.is_empty() && !request_id.is_empty() {
                if !json_mode {
                    eprintln!("\x1b[2m[permission] auto-approved {action}\x1b[0m");
                }
                let _ = host
                    .broker
                    .reply(ask_session, request_id, PermissionReply::Once, None);
            }
            continue;
        }

        let event_session = envelope
            .data
            .get("sessionID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_session != session_id {
            continue;
        }

        if json_mode {
            if let Ok(line) = serde_json::to_string(envelope.as_ref()) {
                println!("{line}");
            }
            match envelope.kind.as_str() {
                "session.execution.succeeded" => return Ok(()),
                "session.execution.failed" => {
                    let message = envelope
                        .data
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("execution failed");
                    bail!("{message}");
                }
                "session.execution.interrupted" => bail!("execution interrupted"),
                _ => continue,
            }
        }

        match envelope.kind.as_str() {
            "session.reasoning.delta" => {
                if let Some(delta) = envelope.data.get("delta").and_then(Value::as_str) {
                    if !in_reasoning {
                        eprint!("\x1b[2m");
                        in_reasoning = true;
                    }
                    eprint!("{delta}");
                    let _ = std::io::stderr().flush();
                }
            }
            "session.reasoning.ended" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                    in_reasoning = false;
                }
            }
            "session.tool.input.started" => {
                if let (Some(id), Some(name)) = (
                    envelope.data.get("id").and_then(Value::as_str),
                    envelope.data.get("name").and_then(Value::as_str),
                ) {
                    tool_names.insert(id.to_owned(), name.to_owned());
                }
            }
            "session.tool.called" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                    in_reasoning = false;
                }
                let id = envelope
                    .data
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let name = tool_names.get(id).map_or("tool", String::as_str);
                let summary = summarize_tool_input(envelope.data.get("input"));
                eprintln!("\x1b[36m▸ {name}\x1b[0m \x1b[2m{summary}\x1b[0m");
            }
            "session.tool.failed" => {
                let err = envelope
                    .data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("tool failed");
                eprintln!("\x1b[31m✗ {err}\x1b[0m");
            }
            "session.text.delta" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                    in_reasoning = false;
                }
                if let Some(delta) = envelope.data.get("delta").and_then(Value::as_str) {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                    printed_text = true;
                }
            }
            "session.text.ended" => {
                if printed_text {
                    println!();
                    printed_text = false;
                }
            }
            "session.execution.succeeded" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                }
                if printed_text {
                    println!();
                }
                return Ok(());
            }
            "session.execution.failed" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                }
                let message = envelope
                    .data
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("execution failed");
                bail!("{message}");
            }
            "session.execution.interrupted" => {
                if in_reasoning {
                    eprintln!("\x1b[0m");
                }
                bail!("execution interrupted");
            }
            _ => {}
        }
    }
    Ok(())
}

fn summarize_tool_input(input: Option<&Value>) -> String {
    let Some(obj) = input.and_then(Value::as_object) else {
        return String::new();
    };
    for key in ["path", "command", "query", "url", "pattern", "id"] {
        if let Some(val) = obj.get(key).and_then(Value::as_str) {
            let single_line = val.lines().next().unwrap_or_default().trim();
            if single_line.chars().count() > 80 {
                let end = single_line
                    .char_indices()
                    .nth(80)
                    .map_or(single_line.len(), |(i, _)| i);
                return format!("{}…", &single_line[..end]);
            }
            return single_line.to_owned();
        }
    }
    String::new()
}

fn print_chat_help() {
    eprintln!("Available commands:");
    eprintln!("  /new                  Start a fresh session");
    eprintln!("  /compact              Compact session transcript into a structured summary");
    eprintln!(
        "  /model <provider/id>  Switch the session model (e.g. anthropic/claude-sonnet-4-5)"
    );
    eprintln!("  /agent <id>           Switch the session agent (e.g. build, plan)");
    eprintln!("  /status               Show current session, agent, model, and message count");
    eprintln!("  /help                 Show this help message");
    eprintln!("  /exit                 Exit interactive chat");
}

async fn print_session_status(host: &TerminalHost, session_id: &str) -> anyhow::Result<()> {
    let session = host
        .services
        .store
        .get_session(session_id)
        .await?
        .unwrap_or(Value::Null);
    let agent = session
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or_else(|| host.services.config.default_agent());
    let model = session
        .get("model")
        .and_then(|m| {
            let p = m.get("providerID")?.as_str()?;
            let id = m.get("id")?.as_str()?;
            Some(format!("{p}/{id}"))
        })
        .or_else(|| host.services.config.model())
        .unwrap_or_else(|| "(catalog default)".to_owned());
    let page = host
        .services
        .store
        .list_messages(oc_store::MessageQuery {
            session_id: session_id.to_owned(),
            limit: 500,
            order: oc_store::Order::Asc,
            cursor: None,
            kind: None,
        })
        .await?;
    eprintln!("Session:  {session_id}");
    eprintln!("Agent:    {agent}");
    eprintln!("Model:    {model}");
    eprintln!("Messages: {}", page.items.len());
    Ok(())
}
