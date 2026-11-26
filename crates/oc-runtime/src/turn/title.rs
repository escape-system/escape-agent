//! Automatic session naming (parity with upstream `SessionTitle`).
//!
//! Runs when a root (`parentID` absent) untitled session finishes promotion of
//! its first prompt, resolves the `title` agent (preferring configured
//! `agents.title.model` / `small_model`, otherwise a minimal-reasoning variant
//! on the active turn model — never switching models automatically), and
//! emits a durable `session.renamed` event if the session is still untitled.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_llm::{LlmEvent, LlmMessage, LlmOptions, LlmRequest, ResolvedModel};
use oc_types::payload::SessionRenamed;
use oc_types::{ModelRef, SessionInfo};
use serde_json::Value;

use crate::prompt;
use crate::runtime::{CancelToken, Runtime};

use super::stream::{
    MAX_KEY_ATTEMPTS, classified_provider_error, classify_llm_error, is_key_rotatable,
};
use super::types::{apply_model_body_overlay, parse_agent_request_overrides};

const MAX_TITLE_CHARS: usize = 100;
const MAX_SNIPPET_CHARS: usize = 4_000;
const MAX_RECENT_MESSAGES: usize = 4;
const TITLE_FINISH_WAIT: Duration = Duration::from_secs(10);
const MINIMAL_REASONING_VARIANTS: [&str; 3] = ["none", "minimal", "low"];
const FALLBACK_TITLE_PROMPT: &str = "You name conversations so the user can find them later. Reply with one short line (3-5 words), sentence case, no quotes or trailing period.";

/// Match `None`, empty, `"New session"`, `"Child session"`, or upstream default
/// `"New session - <ISO>"` / `"Child session - <ISO>"`.
#[must_use]
pub(super) fn is_untitled(title: Option<&str>) -> bool {
    let Some(raw) = title.map(str::trim) else {
        return true;
    };
    if raw.is_empty()
        || raw.eq_ignore_ascii_case("New session")
        || raw.eq_ignore_ascii_case("Child session")
    {
        return true;
    }
    let rest = raw
        .strip_prefix("New session - ")
        .or_else(|| raw.strip_prefix("Child session - "));
    let Some(ts) = rest else {
        return false;
    };
    // Match ISO-8601 UTC millisecond stamps like `2026-11-25T19:43:42.123Z`.
    let b = ts.as_bytes();
    b.len() == 24
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'.'
        && b[23] == b'Z'
}

#[must_use]
pub(super) fn should_generate_title(info: &SessionInfo) -> bool {
    info.parent_id.is_none() && is_untitled(info.title.as_deref())
}

pub(super) async fn maybe_generate(
    runtime: &Runtime,
    info: &SessionInfo,
    turn_ref: &ModelRef,
    turn_model: &ResolvedModel,
    directory: &str,
    cancel: &Arc<CancelToken>,
) {
    if !should_generate_title(info) {
        return;
    }
    let session = info.id.as_str();
    let timer = tokio::time::sleep(TITLE_FINISH_WAIT);
    tokio::select! {
        biased;
        () = cancel.cancelled() => {}
        () = runtime.generate_and_apply_title(session, directory, turn_ref, turn_model, cancel) => {}
        () = timer => {
            tracing::debug!(session = %session, "auto-title timed out; skipping");
        }
    }
}

impl Runtime {
    async fn generate_and_apply_title(
        &self,
        session: &str,
        directory: &str,
        turn_ref: &ModelRef,
        turn_model: &ResolvedModel,
        cancel: &Arc<CancelToken>,
    ) {
        if cancel.is_cancelled() {
            return;
        }
        let Some((system, mut model, overrides)) = self.resolve_title_model(turn_ref, turn_model)
        else {
            return;
        };
        apply_model_body_overlay(&mut model, &overrides.body);

        let Some(messages) = self.title_messages(session).await else {
            return;
        };

        let base_request = LlmRequest {
            model: model.clone(),
            system: vec![system],
            messages,
            tools: Vec::new(),
            options: LlmOptions {
                api_key: None,
                temperature: overrides.temperature,
                top_p: overrides.top_p,
                headers: overrides.headers,
                cache_key: Some(session.to_owned()),
                ..LlmOptions::default()
            },
        };

        let Some(raw_title) = self
            .stream_title_text(&model.provider.id, base_request, cancel)
            .await
        else {
            return;
        };
        let Some(title) = normalize_title(&raw_title) else {
            return;
        };

        // Re-check persisted state so a concurrent manual rename wins.
        let Ok(Some(latest_value)) = self.inner.config.store.get_session(session).await else {
            return;
        };
        let Ok(latest) = Runtime::parse_session(&latest_value) else {
            return;
        };
        if !should_generate_title(&latest) {
            return;
        }

        if let Err(error) = self
            .emit(
                session,
                directory,
                SessionRenamed {
                    session_id: Runtime::session_id(session),
                    title,
                },
            )
            .await
        {
            tracing::warn!(session = %session, %error, "failed to emit session.renamed");
        }
    }

    fn resolve_title_model(
        &self,
        turn_ref: &ModelRef,
        turn_model: &ResolvedModel,
    ) -> Option<(String, ResolvedModel, super::types::AgentRequestOverrides)> {
        let config = &self.inner.config;
        let title_agent = config.config.agent("title")?;
        let system = title_agent
            .get("system")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(FALLBACK_TITLE_PROMPT)
            .to_owned();
        let overrides = parse_agent_request_overrides(&title_agent);
        let configured_ref = title_agent
            .get("model")
            .and_then(prompt::model_ref)
            .or_else(|| {
                config
                    .config
                    .merged()
                    .get("small_model")
                    .and_then(prompt::model_ref)
            });

        let catalog = config.catalog.current();
        let model = if let Some(reference) = configured_ref {
            match config
                .client
                .resolve_model(&catalog, &reference.to_reference())
            {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::debug!(%error, reference = %reference.to_reference(), "configured title model unavailable; skipping auto-title");
                    return None;
                }
            }
        } else {
            select_minimal_same_model(&config.client, &catalog, turn_ref, turn_model)
        };
        Some((system, model, overrides))
    }

    async fn title_messages(&self, session: &str) -> Option<Vec<LlmMessage>> {
        let stored = self.list_all_messages(session).await.ok()?;
        let mut entries: Vec<(bool, String)> = Vec::new();
        for row in &stored {
            let kind = row.get("type").and_then(Value::as_str);
            match kind {
                Some("user") => {
                    if let Some(text) = row.get("text").and_then(Value::as_str) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            entries.push((true, truncate_chars(trimmed, MAX_SNIPPET_CHARS)));
                        }
                    }
                }
                Some("assistant") => {
                    if let Some(text) = assistant_row_text(row) {
                        entries.push((false, truncate_chars(&text, MAX_SNIPPET_CHARS)));
                    }
                }
                _ => {}
            }
        }
        let first_user_idx = entries.iter().position(|(is_user, _)| *is_user)?;
        let mut selected = Vec::new();
        selected.push(LlmMessage::user(entries[first_user_idx].1.clone()));
        let tail = &entries[first_user_idx + 1..];
        let start = tail.len().saturating_sub(MAX_RECENT_MESSAGES);
        for (is_user, text) in &tail[start..] {
            if *is_user {
                selected.push(LlmMessage::user(text.clone()));
            } else {
                selected.push(LlmMessage::assistant(text.clone()));
            }
        }
        Some(selected)
    }

    async fn stream_title_text(
        &self,
        provider: &str,
        base_request: LlmRequest,
        cancel: &Arc<CancelToken>,
    ) -> Option<String> {
        let now_ms = oc_types::now_millis();
        let hook_owns_pool = self
            .inner
            .config
            .client
            .hook()
            .is_some_and(|hook| hook.handles(&base_request));
        let pool = if hook_owns_pool {
            Vec::new()
        } else {
            self.credential_pool(provider, now_ms).await
        };
        let attempts: Vec<Option<oc_llm::PoolKey>> = if pool.is_empty() {
            vec![None]
        } else {
            pool.into_iter().take(MAX_KEY_ATTEMPTS).map(Some).collect()
        };

        for (index, candidate) in attempts.iter().enumerate() {
            if cancel.is_cancelled() {
                return None;
            }
            let mut request = base_request.clone();
            request.options.api_key = candidate.as_ref().map(|entry| entry.key.clone());
            let stream_res = tokio::select! {
                biased;
                () = cancel.cancelled() => return None,
                res = self.inner.model.stream(request) => res,
            };
            let mut stream = match stream_res {
                Ok(stream) => stream,
                Err(error) => {
                    let classified = classify_llm_error(error);
                    if let (Some(credential), Some(kind)) =
                        (candidate.as_ref(), is_key_rotatable(&classified))
                    {
                        self.mark_key_failure(
                            provider,
                            credential.credential_id.as_deref(),
                            kind,
                            now_ms,
                        )
                        .await;
                        if index + 1 < attempts.len() {
                            continue;
                        }
                    }
                    tracing::debug!(error = %classified, "auto-title stream open failed");
                    return None;
                }
            };

            let mut text = String::new();
            let mut failure: Option<String> = None;
            loop {
                let event = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return None,
                    ev = stream.next() => ev,
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    LlmEvent::TextDelta { delta } => text.push_str(&delta),
                    LlmEvent::TextEnd { text: final_text } => text = final_text,
                    LlmEvent::ProviderError { message } => {
                        failure = Some(message);
                        break;
                    }
                    _ => {}
                }
            }
            if let Some(message) = failure {
                let err = classified_provider_error(&message);
                if let (Some(credential), Some(kind)) = (candidate.as_ref(), is_key_rotatable(&err))
                {
                    self.mark_key_failure(
                        provider,
                        credential.credential_id.as_deref(),
                        kind,
                        now_ms,
                    )
                    .await;
                    if index + 1 < attempts.len() {
                        continue;
                    }
                }
                tracing::debug!(error = %message, "auto-title provider error");
                return None;
            }
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        None
    }
}

fn select_minimal_same_model(
    client: &oc_llm::SharedLlmClient,
    catalog: &oc_llm::Catalog,
    turn_ref: &ModelRef,
    turn_model: &ResolvedModel,
) -> ResolvedModel {
    let Some(entry) = catalog.model(&turn_ref.provider_id, &turn_ref.id) else {
        return turn_model.clone();
    };
    let Some(minimal) = MINIMAL_REASONING_VARIANTS.iter().find_map(|candidate| {
        entry
            .variants
            .iter()
            .find(|variant| variant.id == *candidate)
            .map(|variant| variant.id.clone())
    }) else {
        return turn_model.clone();
    };
    if turn_ref.variant.as_deref() == Some(minimal.as_str()) {
        return turn_model.clone();
    }
    let minimal_ref = ModelRef {
        id: turn_ref.id.clone(),
        provider_id: turn_ref.provider_id.clone(),
        variant: Some(minimal),
        credential_id: turn_ref.credential_id.clone(),
    };
    client
        .resolve_model(catalog, &minimal_ref.to_reference())
        .unwrap_or_else(|_| turn_model.clone())
}

fn assistant_row_text(row: &Value) -> Option<String> {
    let parts = row.get("parts").and_then(Value::as_array)?;
    let mut combined = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(text.trim());
        }
    }
    (!combined.is_empty()).then_some(combined)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut end = text.len();
    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == max_chars {
            end = idx;
            break;
        }
    }
    text[..end].to_owned()
}

fn strip_think_blocks(raw: &str) -> String {
    let mut out = raw.to_owned();
    while let Some(start) = out.find("<think>") {
        if let Some(rel_end) = out[start..].find("</think>") {
            let end = start + rel_end + "</think>".len();
            out.replace_range(start..end, "");
        } else {
            out.truncate(start);
            break;
        }
    }
    out
}

pub(super) fn normalize_title(raw: &str) -> Option<String> {
    let cleaned = strip_think_blocks(raw);
    let line = cleaned
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let mut text = line;
    if let Some(rest) = text
        .strip_prefix("Title:")
        .or_else(|| text.strip_prefix("title:"))
    {
        text = rest.trim();
    }
    for quote in ['"', '\'', '`'] {
        if text.len() >= 2 && text.starts_with(quote) && text.ends_with(quote) {
            text = text[1..text.len() - 1].trim();
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches('.').trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_TITLE_CHARS {
        return Some(trimmed.to_owned());
    }
    let prefix = truncate_chars(trimmed, MAX_TITLE_CHARS - 3);
    Some(format!("{}...", prefix.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_untitled_recognizes_empty_and_default_titles() {
        assert!(is_untitled(None));
        assert!(is_untitled(Some("")));
        assert!(is_untitled(Some("   ")));
        assert!(is_untitled(Some("New session")));
        assert!(is_untitled(Some("Child session")));
        assert!(is_untitled(Some("New session - 2026-11-25T19:43:42.123Z")));
        assert!(is_untitled(Some(
            "Child session - 2026-11-25T19:43:42.123Z"
        )));
        assert!(!is_untitled(Some("Fix auth token refresh")));
        assert!(!is_untitled(Some("runtime test")));
    }

    #[test]
    fn normalize_title_strips_thinking_quotes_prefix_and_truncates() {
        assert_eq!(
            normalize_title("<think>reasoning</think>\nTitle: \"Fix `auth.rs` timeout.\""),
            Some("Fix `auth.rs` timeout".to_owned())
        );
        let long = "a".repeat(120);
        let normalized = normalize_title(&long).expect("non-empty");
        assert_eq!(normalized.chars().count(), MAX_TITLE_CHARS);
        assert!(normalized.ends_with("..."));
    }
}
