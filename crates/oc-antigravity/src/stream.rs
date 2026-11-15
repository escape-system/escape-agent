//! Cloud Code Assist SSE stream decoder translating `v1internal:streamGenerateContent`
//! chunks into `oc_llm::LlmEvent` with cumulative thinking deduplication and signature capture.

use crate::accounts::ModelFamily;
use crate::rotation::RateLimitReason;
use crate::signature_cache::SignatureCache;
use oc_llm::{LlmEvent, Usage};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Stateful decoder for Cloud Code Assist SSE byte chunks.
#[derive(Debug)]
pub struct CloudCodeSseDecoder {
    buffer: String,
    text_started: bool,
    full_text: String,
    full_reasoning: String,
    tool_calls_emitted: usize,
    finish_reason: Option<String>,
    usage: Usage,
    emitted_any_output: bool,
    family: ModelFamily,
    session_key: String,
    signature_cache: Option<Arc<Mutex<SignatureCache>>>,
    now_ms: i64,
}

impl CloudCodeSseDecoder {
    /// Create a new SSE decoder bound to a target model family and optional [`SignatureCache`].
    #[must_use]
    pub fn new(
        family: ModelFamily,
        session_key: String,
        signature_cache: Option<Arc<Mutex<SignatureCache>>>,
        now_ms: i64,
    ) -> Self {
        Self {
            buffer: String::new(),
            text_started: false,
            full_text: String::new(),
            full_reasoning: String::new(),
            tool_calls_emitted: 0,
            finish_reason: None,
            usage: Usage::default(),
            emitted_any_output: false,
            family,
            session_key,
            signature_cache,
            now_ms,
        }
    }

    /// True if any meaningful text, reasoning, or tool call has been emitted so far.
    #[must_use]
    pub const fn has_emitted_output(&self) -> bool {
        self.emitted_any_output
    }

    /// Push raw UTF-8 chunk bytes into the decoder and collect newly decoded [`LlmEvent`]s.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<LlmEvent>, StreamDecodeFailure> {
        let s = String::from_utf8_lossy(chunk);
        self.buffer.push_str(&s);
        let mut events = Vec::new();

        while let Some(frame) = pop_next_sse_frame(&mut self.buffer) {
            if let Some(data) = extract_sse_data(&frame) {
                if data == "[DONE]" {
                    continue;
                }
                self.handle_json_frame(&data, &mut events)?;
            }
        }
        Ok(events)
    }

    /// Flush any remaining buffered frame at EOF and emit closing span + `Finish` events.
    pub fn finish(&mut self) -> Result<Vec<LlmEvent>, StreamDecodeFailure> {
        let mut events = Vec::new();
        let rem = std::mem::take(&mut self.buffer);
        if !rem.trim().is_empty()
            && let Some(data) = extract_sse_data(&rem)
            && data != "[DONE]"
        {
            self.handle_json_frame(&data, &mut events)?;
        }

        self.close_text_if_open(&mut events);

        if !self.emitted_any_output {
            return Err(StreamDecodeFailure::EmptyStream);
        }

        let mapped_reason =
            map_finish_reason(self.finish_reason.as_deref(), self.tool_calls_emitted > 0);
        events.push(LlmEvent::Finish {
            reason: mapped_reason,
            usage: self.usage.clone(),
        });
        Ok(events)
    }

    fn handle_json_frame(
        &mut self,
        raw_json: &str,
        events: &mut Vec<LlmEvent>,
    ) -> Result<(), StreamDecodeFailure> {
        let val: Value = serde_json::from_str(raw_json).map_err(|err| {
            StreamDecodeFailure::Fatal(format!("invalid Cloud Code SSE JSON: {err}"))
        })?;

        if let Some(err_obj) = val.get("error") {
            let msg = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream stream error")
                .to_owned();
            if is_thinking_order_error(&msg) {
                return Err(StreamDecodeFailure::ThinkingBlockOrder(msg));
            }
            return Err(classify_in_band_error(msg));
        }

        let inner = val.get("response").unwrap_or(&val);

        if let Some(usage) = inner.get("usageMetadata") {
            update_usage(&mut self.usage, usage);
        }

        let Some(candidate) = inner
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
        else {
            return Ok(());
        };

        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }

        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                self.process_part(part, events);
            }
        }

        Ok(())
    }

    fn process_part(&mut self, part: &Value, events: &mut Vec<LlmEvent>) {
        let sig = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty());

        let is_thought = part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_thought {
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            let delta = compute_dedup_delta(&self.full_reasoning, text);
            if !delta.is_empty() {
                self.close_text_if_open(events);
                self.full_reasoning.push_str(delta);
                self.emitted_any_output = true;
                events.push(LlmEvent::ReasoningDelta {
                    delta: delta.to_owned(),
                });
            }
            if let Some(signature) = sig
                && !self.full_reasoning.trim().is_empty()
                && let Some(ref cache) = self.signature_cache
                && let Ok(mut guard) = cache.lock()
            {
                guard.store(
                    &self.session_key,
                    &self.full_reasoning,
                    signature,
                    self.family,
                    self.now_ms,
                );
            }
            return;
        }

        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            let delta = compute_dedup_delta(&self.full_text, text);
            if !delta.is_empty() {
                if !self.text_started {
                    self.text_started = true;
                    events.push(LlmEvent::TextStart);
                }
                self.full_text.push_str(delta);
                self.emitted_any_output = true;
                events.push(LlmEvent::TextDelta {
                    delta: delta.to_owned(),
                });
            }
        }

        if let Some(fc) = part.get("functionCall")
            && let Some(name) = fc.get("name").and_then(Value::as_str)
        {
            self.close_text_if_open(events);
            self.tool_calls_emitted += 1;
            let id = fc
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || format!("tool_{}", self.tool_calls_emitted),
                    str::to_owned,
                );
            let input = fc
                .get("args")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            self.emitted_any_output = true;
            events.push(LlmEvent::ToolCall {
                id,
                name: name.to_owned(),
                input,
            });
        }
    }

    fn close_text_if_open(&mut self, events: &mut Vec<LlmEvent>) {
        if self.text_started {
            self.text_started = false;
            events.push(LlmEvent::TextEnd {
                text: self.full_text.clone(),
            });
        }
    }
}

/// Failure modes detected while decoding Cloud Code Assist SSE streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamDecodeFailure {
    /// Upstream finished without yielding any text, thinking, or tool call.
    EmptyStream,
    /// Upstream rejected history with thinking-block / signature ordering error.
    ThinkingBlockOrder(String),
    /// In-band SSE error frame reporting a depleted quota window (HTTP status was 200).
    QuotaExhausted(String),
    /// In-band SSE error frame reporting a burst rate limit (HTTP status was 200).
    RateLimited(String),
    /// Non-recoverable upstream stream error.
    Fatal(String),
}

/// Classify an in-band `{"error":{"message":…}}` SSE frame with the shared reason parser so
/// quota/rate-limit frames stay attributable to the serving account instead of failing the turn.
#[must_use]
pub fn classify_in_band_error(message: String) -> StreamDecodeFailure {
    match crate::rotation::parse_rate_limit_reason(200, &message) {
        // `ModelCapacityExhausted` is an account-wide upstream quota stop, so it shares the
        // quota cooldown; `ServerError`/`Unknown` stay fatal (no useful account attribution).
        RateLimitReason::QuotaExhausted | RateLimitReason::ModelCapacityExhausted => {
            StreamDecodeFailure::QuotaExhausted(message)
        }
        RateLimitReason::RateLimitExceeded => StreamDecodeFailure::RateLimited(message),
        RateLimitReason::ServerError | RateLimitReason::Unknown => {
            StreamDecodeFailure::Fatal(message)
        }
    }
}

/// Check if an upstream error message indicates a Claude/Gemini thinking ordering/signature rejection.
#[must_use]
pub fn is_thinking_order_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("thinking_block_order")
        || lower.contains("invalid signature in thinking block")
        || lower.contains("expected `thinking` or `redacted_thinking`")
        || lower.contains("missing thought_signature")
        || lower.contains("thought signature")
}

fn compute_dedup_delta<'a>(accumulated: &str, incoming: &'a str) -> &'a str {
    if !accumulated.is_empty() && incoming.starts_with(accumulated) {
        &incoming[accumulated.len()..]
    } else {
        incoming
    }
}

fn pop_next_sse_frame(buffer: &mut String) -> Option<String> {
    if let Some(pos) = buffer.find("\n\n") {
        let frame = buffer[..pos].to_owned();
        buffer.drain(..pos + 2);
        return Some(frame);
    }
    if let Some(pos) = buffer.find("\r\n\r\n") {
        let frame = buffer[..pos].to_owned();
        buffer.drain(..pos + 4);
        return Some(frame);
    }
    None
}

fn extract_sse_data(frame: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            lines.push(rest.trim_start());
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn update_usage(usage: &mut Usage, meta: &Value) {
    let prompt = meta.get("promptTokenCount").and_then(Value::as_u64);
    let candidates = meta.get("candidatesTokenCount").and_then(Value::as_u64);
    let thoughts = meta.get("thoughtsTokenCount").and_then(Value::as_u64);
    let total = meta.get("totalTokenCount").and_then(Value::as_u64);
    let cached = meta.get("cachedContentTokenCount").and_then(Value::as_u64);

    if let Some(p) = prompt {
        usage.input_tokens = Some(p);
    }
    if candidates.is_some() || thoughts.is_some() {
        usage.output_tokens = Some(candidates.unwrap_or(0) + thoughts.unwrap_or(0));
    }
    if let Some(t) = thoughts {
        usage.reasoning_tokens = Some(t);
    }
    if let Some(tot) = total {
        usage.total_tokens = Some(tot);
    }
    if let Some(c) = cached {
        usage.cache_read_input_tokens = Some(c);
    }
}

fn map_finish_reason(raw: Option<&str>, has_tools: bool) -> String {
    if has_tools {
        return "tool-calls".to_owned();
    }
    match raw {
        Some("MAX_TOKENS") => "length".to_owned(),
        Some("SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST") => {
            "content-filter".to_owned()
        }
        _ => "stop".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_wrapped_thinking_text_tool_and_caches_signature() {
        let sig_cache = Arc::new(Mutex::new(
            SignatureCache::load(None, crate::config::SignatureCacheConfig::default(), 1000)
                .expect("cache"),
        ));
        let mut decoder = CloudCodeSseDecoder::new(
            ModelFamily::Gemini,
            "ses_x".to_owned(),
            Some(sig_cache.clone()),
            1000,
        );

        let sse = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"Thinking...\",\"thoughtSignature\":\"sig_1234567890abcdef\"}]}}]}}\n\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}}\n\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a.txt\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"thoughtsTokenCount\":1,\"totalTokenCount\":6}}}\n\n",
            "data: [DONE]\n\n"
        );

        let mut events = decoder.push_bytes(sse.as_bytes()).expect("push");
        events.extend(decoder.finish().expect("finish"));

        assert!(events.contains(&LlmEvent::ReasoningDelta {
            delta: "Thinking...".to_owned()
        }));
        assert!(events.contains(&LlmEvent::TextDelta {
            delta: "Hello".to_owned()
        }));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ToolCall { name, .. } if name == "read"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::Finish { reason, .. } if reason == "tool-calls"))
        );

        let cached = sig_cache
            .lock()
            .expect("lock")
            .get("ses_x", "Thinking...", 1500)
            .expect("sig cached");
        assert_eq!(cached.signature, "sig_1234567890abcdef");
    }

    #[test]
    fn empty_stream_returns_empty_stream_failure() {
        let mut decoder = CloudCodeSseDecoder::new(ModelFamily::Gemini, "s".to_owned(), None, 1000);
        decoder
            .push_bytes(b"data: {\"response\":{\"candidates\":[]}}\n\ndata: [DONE]\n\n")
            .expect("push");
        assert_eq!(decoder.finish(), Err(StreamDecodeFailure::EmptyStream));
    }
}
