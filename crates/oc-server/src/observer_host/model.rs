//! The model half of the observer: one strict, tool-free JSON verdict call.

use std::sync::Arc;

use futures_util::StreamExt;
use oc_llm::{LlmEvent, LlmMessage, LlmOptions, LlmRequest, ResolvedModel, SharedLlmClient};
use oc_observer::{BoxFuture, ObserverConfig, ObserverError, ObserverModel};
use oc_store::Store;
use serde_json::Value;

use super::VERDICT_MAX_TOKENS;

/// The model half of the observer: one strict JSON request per tick.
#[derive(Debug)]
pub struct LlmObserverModel {
    client: Arc<SharedLlmClient>,
    model: ResolvedModel,
    store: Arc<Store>,
}

impl LlmObserverModel {
    /// Build a model over an already resolved provider/model pair.
    #[must_use]
    pub fn new(client: Arc<SharedLlmClient>, model: ResolvedModel, store: Arc<Store>) -> Self {
        Self {
            client,
            model,
            store,
        }
    }
}

/// Stored credential for `provider`, mirroring the runtime's private
/// `credential_key` (`oc-runtime/src/turn.rs`): the integration's active row
/// wins, otherwise the first non-empty key; OAuth values use `access_token`,
/// other types `key`. Store failures are logged and leave the provider's
/// environment variables to the LLM client.
async fn stored_credential_key(store: &Store, provider: &str) -> Option<String> {
    let credentials = match store.list_credentials().await {
        Ok(credentials) => credentials,
        Err(error) => {
            tracing::warn!(%error, provider, "could not read stored credentials");
            return None;
        }
    };

    let mut fallback = None;
    for entry in credentials {
        if entry.get("integrationID").and_then(Value::as_str) != Some(provider) {
            continue;
        }
        let Some(value) = entry.get("value") else {
            continue;
        };
        let key = match value.get("type").and_then(Value::as_str) {
            Some("oauth") => value.get("access_token").or_else(|| value.get("key")),
            _ => value.get("key").or_else(|| value.get("access_token")),
        }
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty());
        let Some(key) = key else {
            continue;
        };
        if entry.get("active").and_then(Value::as_bool) == Some(true) {
            return Some(key.to_owned());
        }
        fallback.get_or_insert_with(|| key.to_owned());
    }
    fallback
}

impl ObserverModel for LlmObserverModel {
    fn judge<'a>(
        &'a self,
        input: &'a str,
        cfg: &'a ObserverConfig,
    ) -> BoxFuture<'a, Result<String, ObserverError>> {
        Box::pin(async move {
            let api_key = stored_credential_key(&self.store, &self.model.provider.id).await;
            let request = LlmRequest {
                model: self.model.clone(),
                system: vec![system_prompt(cfg.nudge_max_chars)],
                messages: vec![LlmMessage::user(input)],
                tools: Vec::new(),
                options: LlmOptions {
                    temperature: Some(0.0),
                    max_output_tokens: Some(VERDICT_MAX_TOKENS),
                    cache_key: Some("ses_observer".to_owned()),
                    api_key,
                    ..LlmOptions::default()
                },
            };
            let mut stream = self
                .client
                .0
                .stream(request)
                .await
                .map_err(|error| ObserverError::Model(error.to_string()))?;
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    LlmEvent::TextDelta { delta } => text.push_str(&delta),
                    LlmEvent::TextEnd { text: full } => text = full,
                    LlmEvent::ProviderError { message } => {
                        return Err(ObserverError::Model(message));
                    }
                    _ => {}
                }
            }
            if text.trim().is_empty() {
                return Err(ObserverError::Model(
                    "observer model returned no verdict text".to_owned(),
                ));
            }
            Ok(text)
        })
    }
}

/// System prompt: static contract only, no transcript or tool data.
fn system_prompt(nudge_max_chars: usize) -> String {
    format!(
        "You are a supervisor. Return ONLY the JSON verdict described. Do not use tools.\n\n\
         The user message is one compact JSON document about a running session tree \
         (goal, root, active strips, recent strips, tree). Decide whether to intervene.\n\
         Rules for fields:\n\
         - action is required: \"none\" | \"nudge\" | \"inspect\" | \"repair\" | \"goal_done\" | \"goal_blocked\" | \"goal_failed\"\n\
         - target is required except when action is \"none\"\n\
         - reason is required (at most 20 words)\n\
         - message is only for \"nudge\" (must be <= {nudge_max_chars} chars; omit for other actions)\n\
         - repair is only for \"repair\" (omit for other actions)\n\
         - goal_evidence is only for \"goal_done\" (omit for other actions)\n\n\
         Reply with exactly one JSON object with only the applicable fields:\n\
         {{\"action\":\"none|nudge|inspect|repair|goal_done|goal_blocked|goal_failed\",\
         \"target\":\"session or tool-call id\",\
         \"reason\":\"why, at most 20 words\",\
         \"message\":\"nudge text, at most {nudge_max_chars} chars\",\
         \"repair\":{{\"kind\":\"retry_tool|cancel_tool|restart_subagent|compact_context|switch_model|repost\",\"args\":{{}}}},\
         \"goal_evidence\":[\"tool_call_id or message_id\"]}}"
    )
}
