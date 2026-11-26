//! Terminal host bootstrapping, model/agent validation and session plumbing.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use oc_server::AppState;
use oc_store::{Delivery, NewInbox, NewSession, SessionPatch};
use oc_types::{
    EventPayload, InboxItem, MessageId, ModelRef, ServerInfo, ServerPaths, SessionId,
    SessionInboxEnqueued, now_millis, project_id_from_directory,
};
use serde_json::{Value, json};

use crate::bootstrap::{attach_extensions, attach_runtime, open_services};

use super::TerminalHost;

pub(super) async fn boot_terminal_host(
    directory: &Path,
    db: Option<PathBuf>,
) -> anyhow::Result<TerminalHost> {
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
        .with_catalog_cell(services.catalog.clone());
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

pub(super) fn resolve_directory(directory: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let dir = match directory {
        Some(path) => path,
        None => std::env::current_dir().context("resolve working directory")?,
    };
    std::path::absolute(&dir).with_context(|| format!("resolve directory {}", dir.display()))
}

pub(super) fn resolve_prompt_text(parts: &[String]) -> anyhow::Result<String> {
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

pub(super) fn parse_and_validate_model(
    host: &TerminalHost,
    reference: &str,
) -> anyhow::Result<ModelRef> {
    let services = &host.services;
    // Reject malformed references before any resolver runs, so the error names
    // the syntax instead of listing candidates.
    oc_llm::parse_reference(reference)
        .with_context(|| format!("invalid model reference `{reference}` (expected provider/id)"))?;
    // Primary path: the shared client, which also asks the attached transport
    // hook (Antigravity) for models discovered after the startup snapshot.
    let catalog = services.catalog.current();
    let resolved = match host.state.llm() {
        Some(client) => client.resolve_model(&catalog, reference).ok(),
        None => catalog.resolve(reference).ok(),
    };
    // Fallback: the Antigravity service's own merged built-in + discovered
    // catalog. It also understands `#tier` shorthands and un-prefixed ids.
    let resolved = resolved.or_else(|| {
        services
            .antigravity
            .as_ref()
            .and_then(|ag| ag.resolve_catalog_model(&catalog, reference))
    });
    let Some(resolved) = resolved else {
        let error =
            catalog
                .resolve(reference)
                .err()
                .unwrap_or(oc_llm::LlmError::InvalidReference {
                    reference: reference.to_owned(),
                });
        return Err(anyhow::Error::new(error))
            .with_context(|| format!("resolve model `{reference}`"));
    };
    Ok(model_ref_from_resolved(&resolved))
}

/// Normalize a resolved model back to the wire reference, keeping the variant.
///
/// Resolution may canonicalize the id (`google/gemini-3.1-pro` →
/// `google/antigravity-gemini-3.1-pro#low`), so the session must store the
/// canonical `provider/id#variant` rather than the raw parsed input.
fn model_ref_from_resolved(resolved: &oc_llm::ResolvedModel) -> ModelRef {
    ModelRef {
        id: resolved.model.id.clone(),
        provider_id: resolved.provider.id.clone(),
        variant: resolved.variant.as_ref().map(|variant| variant.id.clone()),
        credential_id: None,
    }
}

pub(super) fn validate_agent(
    config: &oc_config::ConfigService,
    agent_id: &str,
) -> anyhow::Result<()> {
    if config.agent(agent_id).is_none() {
        bail!("Agent not found: `{agent_id}`");
    }
    Ok(())
}

pub(super) async fn ensure_session(
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

pub(super) async fn enqueue_user_prompt(
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

pub(super) async fn enqueue_compaction(
    state: &AppState,
    session_id: &str,
) -> anyhow::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_model_reference_keeps_the_variant() {
        let snapshot = json!({
            "test": {
                "id": "test",
                "name": "Test Provider",
                "env": [],
                "models": {
                    "alpha": {
                        "id": "alpha",
                        "type": "chat",
                        "name": "Alpha",
                        "release_date": "2025-01-02",
                        "modalities": {"input": ["text"], "output": ["text"]},
                        "capabilities": {
                            "tools": {"supported": true},
                            "reasoning": {"supported": true, "effort": ["low", "high"]},
                            "temperature": true
                        },
                        "limit": {"context": 100_000, "output": 10_000},
                        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
                        "api": {
                            "chat-completions": {
                                "base_url": "http://unused.invalid",
                                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
                            }
                        }
                    }
                }
            }
        });
        let catalog = oc_llm::Catalog::from_snapshot(&snapshot.to_string()).expect("catalog");
        let plain = catalog.resolve("test/alpha").expect("plain resolves");
        assert_eq!(
            model_ref_from_resolved(&plain),
            ModelRef {
                id: "alpha".to_owned(),
                provider_id: "test".to_owned(),
                variant: None,
                credential_id: None,
            }
        );
        let variant = catalog.resolve("test/alpha#low").expect("variant resolves");
        assert_eq!(
            model_ref_from_resolved(&variant),
            ModelRef {
                id: "alpha".to_owned(),
                provider_id: "test".to_owned(),
                variant: Some("low".to_owned()),
                credential_id: None,
            }
        );
    }
}
