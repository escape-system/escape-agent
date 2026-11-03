//! Prompt admission: `/prompt`, `/synthetic`, `/compact` and `/command`.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::response::Response;
use oc_store::{Delivery, NewInbox, NewMessage, SessionPatch};
use oc_types::{ApiError, MessageId, now_millis};
use serde_json::{Map, Value, json};

use super::bodies::{CommandBody, CompactBody, PromptBody, SyntheticBody};
use super::{notify, publish_enqueued};
use crate::AppState;
use crate::data::events::publish_durable;
use crate::data::{
    delivery_from_wire, no_content, require_config, require_store, schema_error, session_not_found,
};
use crate::error::ErrorResponse;

fn parse_mode_slash_command(raw: &str) -> Option<(&'static str, Option<u32>, &str)> {
    let trimmed = raw.trim();
    for (prefix, mode) in [("/complex", "complex"), ("/basic", "basic")] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if rest.is_empty() {
                return Some((mode, None, ""));
            }
            if rest.starts_with(char::is_whitespace) {
                let rest = rest.trim();
                if let Some(first_word) = rest.split_whitespace().next()
                    && let Ok(depth) = first_word.parse::<u32>()
                    && (1..=10).contains(&depth)
                {
                    let remainder = rest[first_word.len()..].trim();
                    return Some((mode, Some(depth), remainder));
                }
                return Some((mode, None, rest));
            }
        }
    }
    None
}

async fn apply_subagent_mode(
    state: &AppState,
    session_id: &str,
    mode: &str,
    custom_depth: Option<u32>,
) -> Result<(), ErrorResponse> {
    let store = require_store(state)?;
    let session = store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id))?;
    let root_id = store
        .session_node(session_id)
        .await?
        .map_or_else(|| session_id.to_owned(), |node| node.root_id);
    let max_depth = custom_depth.unwrap_or_else(|| if mode == "complex" { 3 } else { 1 });

    let mut targets = vec![session_id.to_owned()];
    if root_id != session_id {
        targets.push(root_id);
    }
    for target_id in targets {
        let current = if target_id == session_id {
            Some(session.clone())
        } else {
            store.get_session(&target_id).await?
        };
        let mut metadata = current
            .as_ref()
            .and_then(|value| value.get("metadata"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert("subagentMode".to_owned(), Value::String(mode.to_owned()));
        metadata.insert("maxDepth".to_owned(), json!(max_depth));
        store
            .patch_session(
                &target_id,
                SessionPatch {
                    metadata: Some(Value::Object(metadata.clone())),
                    ..SessionPatch::default()
                },
            )
            .await?;
        publish_durable(
            state,
            &target_id,
            "session.metadata.updated",
            json!({
                "sessionID": target_id,
                "metadata": metadata,
            }),
            1,
        )
        .await?;
    }

    let notice = if mode == "complex" {
        format!(
            "Subagent mode switched to /complex ({max_depth}-level orchestrator: max_depth = {max_depth}; root agent delegates via subagent and coordinator agents)."
        )
    } else {
        format!(
            "Subagent mode switched to /basic (single-level subagents: max_depth = {max_depth})."
        )
    };
    let now = now_millis();
    store
        .append_message(NewMessage {
            id: MessageId::new().into_inner(),
            session_id: session_id.to_owned(),
            kind: "system".to_owned(),
            data: json!({
                "text": notice,
                "description": format!("Instructions updated: subagentMode ({mode})"),
                "time": { "created": now },
            }),
            time_created: now,
        })
        .await?;
    publish_durable(
        state,
        session_id,
        "session.instructions.updated",
        json!({
            "sessionID": session_id,
            "delta": { "subagentMode": mode },
            "text": notice,
        }),
        2,
    )
    .await?;
    Ok(())
}

/// `POST /api/session/:sid/prompt`
pub async fn prompt(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<PromptBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "text must not be empty").into());
    }
    // Raw `/compress` compatibility (the web app also exposes it through
    // `GET /api/command`): the text never reaches the model and no user message
    // is admitted, exactly like the upstream `/compact` command.
    if body.text.trim().eq_ignore_ascii_case("/compress") {
        let item = admit_compaction(&state, &session_id, body.id, body.delivery).await?;
        return Ok(Json(json!({ "data": item })));
    }
    if let Some((mode, depth_override, rest)) = parse_mode_slash_command(&body.text) {
        apply_subagent_mode(&state, &session_id, mode, depth_override).await?;
        if rest.is_empty() {
            let id = body
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| MessageId::new().into_inner());
            let delivery = delivery_from_wire(body.delivery.as_deref())?;
            return Ok(Json(json!({
                "data": {
                    "id": id,
                    "sessionID": session_id,
                    "time": { "created": now_millis() },
                    "type": "user",
                    "payload": { "text": body.text },
                    "delivery": delivery,
                }
            })));
        }
        let mut extra = Map::new();
        if let Some(files) = body.files {
            extra.insert(
                "files".to_owned(),
                Value::Array(normalize_prompt_files(files)),
            );
        }
        if let Some(agents) = body.agents {
            extra.insert("agents".to_owned(), Value::Array(agents));
        }
        if let Some(skills) = body.skills {
            extra.insert("skills".to_owned(), Value::Array(skills));
        }
        if let Some(metadata) = body.metadata {
            extra.insert("metadata".to_owned(), Value::Object(metadata));
        }
        let item = admit(
            &state,
            &session_id,
            body.id,
            rest.to_owned(),
            extra,
            body.delivery,
        )
        .await?;
        return Ok(Json(json!({ "data": item })));
    }
    let mut extra = Map::new();
    if let Some(files) = body.files {
        extra.insert(
            "files".to_owned(),
            Value::Array(normalize_prompt_files(files)),
        );
    }
    if let Some(agents) = body.agents {
        extra.insert("agents".to_owned(), Value::Array(agents));
    }
    if let Some(skills) = body.skills {
        extra.insert("skills".to_owned(), Value::Array(skills));
    }
    if let Some(metadata) = body.metadata {
        extra.insert("metadata".to_owned(), Value::Object(metadata));
    }
    let item = admit(
        &state,
        &session_id,
        body.id,
        body.text,
        extra,
        body.delivery,
    )
    .await?;
    Ok(Json(json!({ "data": item })))
}

/// Normalizes incoming prompt file attachments into canonical `PromptFileAttachment` objects.
///
/// Converts client `PromptInput.FileAttachment` records carrying a `uri` (such as inline
/// data URLs like `data:image/png;base64,...` or external URIs) into the stored representation
/// with `data`, `mime`, and `source` (`{ type: "inline" }` or `{ type: "uri", uri }`).
fn normalize_prompt_files(files: Vec<Value>) -> Vec<Value> {
    files
        .into_iter()
        .map(|val| {
            if let Value::Object(mut map) = val {
                if map.contains_key("data")
                    && map.contains_key("mime")
                    && map.contains_key("source")
                {
                    return Value::Object(map);
                }

                let name = map
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let uri = map
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();

                if uri.starts_with("data:") {
                    let (header, data_part) = uri.split_once(',').unwrap_or(("", &uri));
                    let header_rest = header.strip_prefix("data:").unwrap_or(header);
                    let mut mime_candidate = header_rest
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if mime_candidate.is_empty() {
                        if let Some(ref n) = name {
                            mime_candidate =
                                mime_guess::from_path(n).first_or_octet_stream().to_string();
                        } else {
                            "application/octet-stream".clone_into(&mut mime_candidate);
                        }
                    }
                    map.remove("uri");
                    map.insert("data".to_owned(), Value::String(data_part.to_owned()));
                    map.insert("mime".to_owned(), Value::String(mime_candidate));
                    map.insert("source".to_owned(), json!({ "type": "inline" }));
                    return Value::Object(map);
                }

                if !uri.is_empty() {
                    let mime = map.get("mime").and_then(Value::as_str).map_or_else(
                        || {
                            if let Some(ref n) = name {
                                mime_guess::from_path(n).first_or_octet_stream().to_string()
                            } else {
                                mime_guess::from_path(&uri)
                                    .first_or_octet_stream()
                                    .to_string()
                            }
                        },
                        ToOwned::to_owned,
                    );
                    let data = map
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    map.remove("uri");
                    map.insert("data".to_owned(), Value::String(data));
                    map.insert("mime".to_owned(), Value::String(mime));
                    map.insert("source".to_owned(), json!({ "type": "uri", "uri": uri }));
                    return Value::Object(map);
                }

                if map.contains_key("data") {
                    if !map.contains_key("mime") {
                        let mime = if let Some(ref n) = name {
                            mime_guess::from_path(n).first_or_octet_stream().to_string()
                        } else {
                            "application/octet-stream".to_owned()
                        };
                        map.insert("mime".to_owned(), Value::String(mime));
                    }
                    if !map.contains_key("source") {
                        map.insert("source".to_owned(), json!({ "type": "inline" }));
                    }
                    return Value::Object(map);
                }

                Value::Object(map)
            } else {
                val
            }
        })
        .collect()
}

/// Durably admit one user prompt, then wake the runtime.
///
/// Only `steer` input is materialised as a message here: the running turn reads
/// the transcript from the store at its next step, so the row must already be
/// durable before `notify`. `queue` input is only written to the inbox; the
/// runtime promotes that row when the current turn settles, which is what keeps
/// a queued prompt invisible to the turn that was already running.
///
/// Ordering for both modes is store → event → notify.
async fn admit(
    state: &AppState,
    session_id: &str,
    id: Option<String>,
    text: String,
    extra: Map<String, Value>,
    delivery: Option<String>,
) -> Result<Value, ErrorResponse> {
    let store = require_store(state)?;
    store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id))?;

    let delivery = delivery_from_wire(delivery.as_deref())?;
    let id = id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| MessageId::new().into_inner());
    if store.get_message(session_id, &id).await?.is_some() {
        return Err(ApiError::conflict(format!(
            "Prompt message ID conflicts with an existing durable record: {id}"
        ))
        .into());
    }

    let mut payload = Map::new();
    payload.insert("text".to_owned(), Value::String(text));
    payload.extend(extra);
    let payload = Value::Object(payload);

    // Only `steer` input is materialised; `queue` rows stay inbox-only and the
    // runtime promotes them when the current turn settles. Writing the message
    // here would leak the text into the running turn's next request and run
    // the prompt twice.
    let mut message_data = None;
    if delivery == Delivery::Steer {
        let mut data = payload.clone();
        if let Value::Object(map) = &mut data {
            map.insert("time".to_owned(), json!({ "created": now_millis() }));
        }
        message_data = Some(data);
    }

    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: "user".to_owned(),
            payload,
            delivery,
        })
        .await?;

    if let Some(data) = message_data
        && let Err(error) = store
            .append_message(NewMessage {
                id: id.clone(),
                session_id: session_id.to_owned(),
                kind: "user".to_owned(),
                data,
                time_created: now_millis(),
            })
            .await
    {
        let _ = store.cancel_inbox(session_id, &id).await;
        return Err(error.into());
    }

    publish_enqueued(state, session_id, &id, &item).await?;
    notify(state, session_id).await?;
    Ok(item)
}

/// `POST /api/session/:sid/synthetic`
pub async fn synthetic(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<SyntheticBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "text must not be empty").into());
    }
    let store = require_store(&state)?;
    store
        .get_session(&session_id)
        .await?
        .ok_or_else(|| session_not_found(&session_id))?;

    let delivery = delivery_from_wire(body.delivery.as_deref())?;
    let id = body
        .id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| MessageId::new().into_inner());
    if store.get_message(&session_id, &id).await?.is_some() {
        return Err(ApiError::conflict(format!(
            "Synthetic message ID conflicts with an existing durable record: {id}"
        ))
        .into());
    }

    let mut payload = Map::new();
    payload.insert("text".to_owned(), Value::String(body.text));
    if let Some(description) = body.description {
        payload.insert("description".to_owned(), Value::String(description));
    }
    if let Some(metadata) = body.metadata {
        payload.insert("metadata".to_owned(), Value::Object(metadata));
    }

    // Synthetic input is never materialised here, whatever its delivery: the
    // runtime promotes the inbox row (mid-turn for `steer`, at settle for
    // `queue`), so a queued row cannot leak into the running turn.
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.clone(),
            kind: "synthetic".to_owned(),
            payload: Value::Object(payload),
            delivery,
        })
        .await?;
    publish_enqueued(&state, &session_id, &id, &item).await?;
    notify(&state, &session_id).await?;
    Ok(Json(json!({ "data": item })))
}

/// Admit one `compaction` inbox row and wake the runtime.
///
/// Shared by `POST /compact`, `POST /command {"name":"compress"}` and the
/// raw `/compress` prompt alias, so all three produce exactly the same durable
/// row and event ordering as the upstream manual-compaction endpoint.
async fn admit_compaction(
    state: &AppState,
    session_id: &str,
    id: Option<String>,
    delivery: Option<String>,
) -> Result<Value, ErrorResponse> {
    let store = require_store(state)?;
    store
        .get_session(session_id)
        .await?
        .ok_or_else(|| session_not_found(session_id))?;

    let delivery = delivery_from_wire(delivery.as_deref())?;
    let id = id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| MessageId::new().into_inner());
    let item = store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session_id.to_owned(),
            kind: "compaction".to_owned(),
            payload: json!({}),
            delivery,
        })
        .await?;
    publish_enqueued(state, session_id, &id, &item).await?;
    notify(state, session_id).await?;
    Ok(item)
}

/// `POST /api/session/:sid/compact`
pub async fn compact(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<CompactBody>, JsonRejection>,
) -> Result<Json<Value>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let item = admit_compaction(&state, &session_id, body.id, body.delivery).await?;
    Ok(Json(json!({ "data": item })))
}

/// `POST /api/session/:sid/command`
pub async fn command(
    Path(session_id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<CommandBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let Some(name) = body.name.filter(|name| !name.trim().is_empty()) else {
        return Err(ApiError::new(400, "BadRequestError", "name must not be empty").into());
    };
    let config = require_config(&state)?;
    // Builtin `/compress` (upstream calls the same operation `/compact`).
    // A markdown command named `compress` keeps precedence.
    if name.eq_ignore_ascii_case("compress") && config.command(&name).is_none() {
        admit_compaction(&state, &session_id, None, body.delivery).await?;
        return Ok(no_content());
    }
    if (name.eq_ignore_ascii_case("complex") || name.eq_ignore_ascii_case("basic"))
        && config.command(&name).is_none()
    {
        let mode = if name.eq_ignore_ascii_case("complex") {
            "complex"
        } else {
            "basic"
        };
        let mut arguments = body.text.unwrap_or_default();
        let mut depth_override = None;
        if let Some(first_word) = arguments.split_whitespace().next()
            && let Ok(depth) = first_word.parse::<u32>()
            && (1..=10).contains(&depth)
        {
            depth_override = Some(depth);
            arguments = arguments[first_word.len()..].trim().to_owned();
        }
        apply_subagent_mode(&state, &session_id, mode, depth_override).await?;
        if !arguments.trim().is_empty() {
            let mut extra = Map::new();
            if let Some(files) = body.files {
                extra.insert(
                    "files".to_owned(),
                    Value::Array(normalize_prompt_files(files)),
                );
            }
            if let Some(agents) = body.agents {
                extra.insert("agents".to_owned(), Value::Array(agents));
            }
            if let Some(skills) = body.skills {
                extra.insert("skills".to_owned(), Value::Array(skills));
            }
            admit(
                &state,
                &session_id,
                None,
                arguments.trim().to_owned(),
                extra,
                body.delivery,
            )
            .await?;
        }
        return Ok(no_content());
    }
    let Some(command) = config.command(&name) else {
        return Err(ApiError::not_found(
            "CommandNotFoundError",
            format!("Command not found: {name}"),
        )
        .with("command", json!(name))
        .into());
    };
    let arguments = body.text.unwrap_or_default();
    let text = if command.template.contains("$ARGUMENTS") {
        command.template.replace("$ARGUMENTS", &arguments)
    } else if arguments.trim().is_empty() {
        command.template.clone()
    } else {
        format!("{}\n\n{}", command.template.trim_end(), arguments)
    };
    if text.trim().is_empty() {
        return Err(ApiError::new(400, "BadRequestError", "text must not be empty").into());
    }
    let mut extra = Map::new();
    if let Some(files) = body.files {
        extra.insert(
            "files".to_owned(),
            Value::Array(normalize_prompt_files(files)),
        );
    }
    if let Some(agents) = body.agents {
        extra.insert("agents".to_owned(), Value::Array(agents));
    }
    if let Some(skills) = body.skills {
        extra.insert("skills".to_owned(), Value::Array(skills));
    }
    admit(&state, &session_id, None, text, extra, body.delivery).await?;
    Ok(no_content())
}
