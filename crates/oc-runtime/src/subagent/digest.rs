use std::fmt::Write as _;

use oc_store::{MessageQuery, Order};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::runtime::Runtime;

use super::protocol::message_text;
use super::settings::settings_for_session;
use super::state::SubagentState;
use super::tree::{authorize_digest, load_info, optional_str, store_error};

/// Digest cache capacity per runtime.
pub(super) const DIGEST_CACHE_CAPACITY: usize = 64;
/// Digest requests allowed per caller per window.
const DIGEST_RATE_LIMIT: usize = 30;
/// Digest rate window in milliseconds.
const DIGEST_RATE_WINDOW_MS: i64 = 600_000;
/// Maximum transcript messages read by `digest`.
const DIGEST_MESSAGES: usize = 40;
/// Characters reserved for the child's complete final answer.
const DIGEST_FINAL_ANSWER_BUDGET: usize = 6_000;
/// Characters reserved for the excerpted recent-context section.
const DIGEST_CONTEXT_BUDGET: usize = 10_000;
/// Per-message cap for user, peer, and synthetic messages in recent context.
const DIGEST_CONTEXT_MESSAGE_CAP: usize = 400;
/// Per-message cap for non-final assistant messages in recent context.
const DIGEST_CONTEXT_ASSISTANT_CAP: usize = 400;

/// Maximum parent hops when authorizing a digest target against the caller's
/// subtree.
pub(super) const DIGEST_ANCESTOR_STEPS: usize = 128;

/// Characters per token used only to bound the digest answer.
const CHARS_PER_TOKEN: usize = 4;
/// Guards ancestor walks against corrupt trees.
pub(super) const MAX_ANCESTOR_STEPS: usize = 1024;

#[derive(Debug, Clone)]
pub(super) struct DigestTool {
    pub(super) runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for DigestTool {
    fn name(&self) -> &'static str {
        "digest"
    }

    fn description(&self) -> &'static str {
        "Summarize a session transcript with the scribe model. Never returns raw tool output."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Session to summarize; defaults to this session." },
                "tool_call_id": { "type": "string", "description": "Focus on one tool call." },
                "question": { "type": "string", "description": "What to answer about the transcript." },
                "max_tokens": { "type": "integer", "minimum": 1, "description": "Answer budget, default 300." }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let target = optional_str(&input, "session_id").unwrap_or_else(|| caller.clone());
        let question = optional_str(&input, "question")
            .unwrap_or_else(|| "Summarize what this session did and any errors.".to_owned());
        let tool_call_id = optional_str(&input, "tool_call_id");
        let max_tokens = input
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(1, 4096);
        let settings = settings_for_session(runtime, &caller).await;
        let scribe = runtime.scribe_model().ok_or_else(|| {
            let configured = settings
                .digest_model
                .as_deref()
                .map_or(String::new(), |model| format!(" (configured {model})"));
            ToolError::new(format!(
                "DigestUnavailableError: digest requires a scribe model{configured}"
            ))
        })?;
        load_info(runtime, &target).await?;
        authorize_digest(runtime, &caller, &target).await?;
        let page = runtime
            .inner
            .config
            .store
            .list_messages(MessageQuery {
                session_id: target.clone(),
                limit: DIGEST_MESSAGES,
                order: Order::Desc,
                cursor: None,
                kind: None,
            })
            .await
            .map_err(|error| store_error(&error))?;
        let excerpts = digest_excerpts(&page.items);
        let hash = digest_hash(&target, tool_call_id.as_deref(), &question, &excerpts);
        let key = format!("{target}:{hash}");
        if let Some(cached) = runtime
            .inner
            .subagents
            .lock()
            .expect("subagent state")
            .digests
            .get(&key)
        {
            // The cache stores the raw model answer, so each caller still gets
            // its own token budget instead of the first caller's truncation.
            let text = truncate_tokens(&cached, max_tokens);
            return Ok(ToolResult::text(text).with_metadata(json!({ "cached": true })));
        }
        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            check_digest_rate(&mut state, &caller)?;
        }
        let prompt = format!(
            "Answer the question about this session transcript. Be concise and factual. \
             Never quote raw tool output verbatim.\n\
             The transcript is excerpted by design: the FINAL ANSWER section holds the \
             session's complete latest answer, while RECENT CONTEXT is a shortened excerpt \
             of earlier messages. Answer from what is present and do not refuse, apologize, \
             or mention truncation just because the context is excerpted.\n\
             Question: {question}\n\
             Tool call: {}\n\nTranscript:\n{excerpts}",
            tool_call_id.as_deref().unwrap_or("(none)")
        );
        let response = match scribe.complete(&prompt).await {
            Ok(resp) if !resp.trim().is_empty() => resp,
            Ok(_) | Err(_) => format!("(Summary: {question})\n{excerpts}"),
        };
        let text = truncate_tokens(&response, max_tokens);
        runtime
            .inner
            .subagents
            .lock()
            .expect("subagent state")
            .digests
            .put(key, response);
        Ok(ToolResult::text(text).with_metadata(json!({ "cached": false })))
    }
}

fn check_digest_rate(state: &mut SubagentState, caller: &str) -> Result<(), ToolError> {
    let now = oc_types::now_millis();
    let window = state.digest_rate.entry(caller.to_owned()).or_default();
    while window
        .front()
        .is_some_and(|stamp| now.saturating_sub(*stamp) >= DIGEST_RATE_WINDOW_MS)
    {
        window.pop_front();
    }
    if window.len() >= DIGEST_RATE_LIMIT {
        return Err(ToolError::new(format!(
            "DigestRateLimitError: digest is limited to {DIGEST_RATE_LIMIT} requests per 10 minutes",
        )));
    }
    window.push_back(now);
    Ok(())
}

fn digest_hash(
    session: &str,
    tool_call_id: Option<&str>,
    question: &str,
    excerpts: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    hasher.update(b"\0");
    hasher.update(tool_call_id.unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(question.as_bytes());
    hasher.update(b"\0");
    hasher.update(excerpts.as_bytes());
    hex::encode(hasher.finalize())
}

fn truncate_tokens(text: &str, max_tokens: u64) -> String {
    let limit = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(CHARS_PER_TOKEN);
    text.chars().take(limit).collect()
}

fn digest_excerpts(messages: &[Value]) -> String {
    // `messages` arrive newest-first (`Order::Desc`), so the first assistant
    // message with text is the session's final answer.
    let final_index = messages.iter().position(|message| {
        message.get("type").and_then(Value::as_str) == Some("assistant")
            && !message_text(message).trim().is_empty()
    });

    let mut out = String::new();
    if let Some(index) = final_index {
        let final_text = message_text(&messages[index]);
        out.push_str("FINAL ANSWER (complete):\n");
        push_bounded(
            &mut out,
            final_text.trim(),
            DIGEST_FINAL_ANSWER_BUDGET,
            &format!("…[final answer truncated at {DIGEST_FINAL_ANSWER_BUDGET} chars]"),
        );
        out.push('\n');
    }

    let mut context = String::new();
    let mut omitted = false;
    for (index, message) in messages.iter().enumerate() {
        if Some(index) == final_index {
            continue;
        }
        let mut entry = String::new();
        let kind = message.get("type").and_then(Value::as_str).unwrap_or("?");
        match kind {
            "user" | "synthetic" | "peer" => {
                entry.push_str(kind);
                entry.push_str(": ");
                push_bounded(
                    &mut entry,
                    message_text(message).trim(),
                    DIGEST_CONTEXT_MESSAGE_CAP,
                    "…[truncated]",
                );
                entry.push('\n');
            }
            "assistant" => {
                let text = message_text(message);
                if !text.trim().is_empty() {
                    entry.push_str("assistant: ");
                    push_bounded(
                        &mut entry,
                        text.trim(),
                        DIGEST_CONTEXT_ASSISTANT_CAP,
                        "…[truncated]",
                    );
                    entry.push('\n');
                }
                if let Some(parts) = message.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) != Some("tool") {
                            continue;
                        }
                        let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let state = part
                            .get("state")
                            .and_then(|state| state.get("status"))
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let _ = writeln!(entry, "tool {name}: {state}");
                    }
                }
            }
            "idle" => {
                let outcome = message
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("idle");
                let _ = writeln!(entry, "idle: {outcome}");
            }
            _ => {}
        }
        if context.chars().count() + entry.chars().count() > DIGEST_CONTEXT_BUDGET {
            omitted = true;
            break;
        }
        context.push_str(&entry);
    }

    if !context.is_empty() || omitted {
        out.push_str("RECENT CONTEXT (excerpted):\n");
        out.push_str(&context);
        if omitted {
            let _ = writeln!(
                out,
                "…[older messages omitted beyond {DIGEST_CONTEXT_BUDGET} chars]"
            );
        }
    }
    out
}

/// Appends `text`, cutting it at `limit` characters when it exceeds the budget
/// and marking the cut explicitly.
fn push_bounded(out: &mut String, text: &str, limit: usize, marker: &str) {
    if text.chars().count() <= limit {
        out.push_str(text);
        return;
    }
    out.extend(text.chars().take(limit));
    out.push_str(marker);
}
