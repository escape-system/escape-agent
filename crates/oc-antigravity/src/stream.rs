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
    raw_bytes: Vec<u8>,
    buffer: String,
    text_started: bool,
    full_text: String,
    full_reasoning: String,
    tool_calls_emitted: usize,
    finish_reason: Option<String>,
    prompt_block_reason: Option<String>,
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
            raw_bytes: Vec::new(),
            buffer: String::new(),
            text_started: false,
            full_text: String::new(),
            full_reasoning: String::new(),
            tool_calls_emitted: 0,
            finish_reason: None,
            prompt_block_reason: None,
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
        append_utf8_chunk(&mut self.raw_bytes, chunk, &mut self.buffer);
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
        if !self.raw_bytes.is_empty() {
            let tail = std::mem::take(&mut self.raw_bytes);
            self.buffer.push_str(&String::from_utf8_lossy(&tail));
        }
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
            if let Some(reason) = self.prompt_block_reason.take() {
                return Err(StreamDecodeFailure::Fatal(format!(
                    "Cloud Code blocked prompt ({reason})"
                )));
            }
            if let Some(reason) = self.finish_reason.as_deref()
                && matches!(
                    reason,
                    "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII"
                )
            {
                return Err(StreamDecodeFailure::Fatal(format!(
                    "Cloud Code finished without output ({reason})"
                )));
            }
            tracing::warn!(
                reason = "no text, reasoning, or tool-call output in stream",
                finish_reason = ?self.finish_reason,
                prompt_tokens = ?self.usage.input_tokens,
                thoughts_tokens = ?self.usage.reasoning_tokens,
                text_len = self.full_text.len(),
                reasoning_len = self.full_reasoning.len(),
                tool_calls = self.tool_calls_emitted,
                "post-event EmptyStream"
            );
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

        let inner = val.get("response").unwrap_or(&val);

        if let Some(err_obj) = val.get("error").or_else(|| inner.get("error")) {
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

        if let Some(feedback) = inner.get("promptFeedback")
            && let Some(reason) = feedback.get("blockReason").and_then(Value::as_str)
        {
            let detail = feedback
                .get("blockReasonMessage")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or_else(String::new, |msg| format!(": {msg}"));
            self.prompt_block_reason = Some(format!("{reason}{detail}"));
        }

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
            self.process_function_call_part(fc, name, sig, events);
        }
    }

    fn process_function_call_part(
        &mut self,
        fc: &Value,
        name: &str,
        sig: Option<&str>,
        events: &mut Vec<LlmEvent>,
    ) {
        self.close_text_if_open(events);
        self.tool_calls_emitted += 1;
        // Preserve the provider's real tool-call id verbatim. Empty/missing ids must
        // NOT be collapsed by name: keep each call distinct with a per-call marker.
        let id = fc
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || format!("unnamed-call-{}", self.tool_calls_emitted),
                str::to_owned,
            );
        let input = fc
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if let Some(signature) = sig
            && let Some(ref cache) = self.signature_cache
            && let Ok(mut guard) = cache.lock()
        {
            guard.store(
                &self.session_key,
                &format!("__toolcall__:{id}"),
                signature,
                self.family,
                self.now_ms,
            );
            if let Ok(args_json) = serde_json::to_string(&input) {
                guard.store(
                    &self.session_key,
                    &format!("__toolcall_args__:{name}:{args_json}"),
                    signature,
                    self.family,
                    self.now_ms,
                );
            }
        }
        let input_text = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned());
        self.emitted_any_output = true;
        events.push(LlmEvent::ToolInputStart {
            id: id.clone(),
            name: name.to_owned(),
        });
        events.push(LlmEvent::ToolInputEnd {
            id: id.clone(),
            text: input_text,
        });
        events.push(LlmEvent::ToolCall {
            id,
            name: name.to_owned(),
            input,
        });
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

/// Check if an upstream error message indicates a thinking-block / thought-signature
/// ordering rejection. A bare mention is insufficient; require explicit order-error phrasing.
#[must_use]
pub fn is_thinking_order_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("thinking_block_order")
        || lower.contains("thinking block order")
        || lower.contains("expected `thinking` or `redacted_thinking`")
    {
        return true;
    }
    let has_signature_term = lower.contains("thought_signature")
        || lower.contains("thought signature")
        || lower.contains("signature in thinking block");
    let has_invalid_signature = lower.contains("invalid signature")
        || lower.contains("invalid thought_signature")
        || lower.contains("invalid thought signature");
    has_signature_term
        && (lower.contains("order")
            || lower.contains("out of sequence")
            || lower.contains("wrong position")
            || lower.contains("missing")
            || lower.contains("corrupt")
            || has_invalid_signature)
}

/// Minimum overlap length (in characters) required before frames are merged.
const DEDUP_MIN_OVERLAP_CHARS: usize = 12;
/// Upper bound on the overlap scan window (in bytes) for large accumulated strings.
const DEDUP_MAX_SCAN_BYTES: usize = 64 * 1024;

/// Outcome of comparing a cumulative snapshot with the text already emitted on a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DedupDecision<'a> {
    /// Append this strict suffix (may be empty for an identical frame).
    Emit(&'a str),
    /// The frame is a revision of already-emitted text; emit nothing and keep the old text.
    Dropped,
}

fn compute_dedup_delta<'a>(accumulated: &str, incoming: &'a str) -> &'a str {
    match dedup_decision(accumulated, incoming) {
        DedupDecision::Emit(delta) => delta,
        DedupDecision::Dropped => "",
    }
}

fn dedup_decision<'a>(accumulated: &str, incoming: &'a str) -> DedupDecision<'a> {
    if accumulated.is_empty() || incoming.is_empty() {
        return DedupDecision::Emit(incoming);
    }
    if let Some(rest) = incoming.strip_prefix(accumulated) {
        return DedupDecision::Emit(rest);
    }
    // Bounded tail of the accumulated text; step back to a valid UTF-8 boundary (MSRV-safe).
    let mut scan_start = accumulated.len().saturating_sub(DEDUP_MAX_SCAN_BYTES);
    while scan_start > 0 && !accumulated.is_char_boundary(scan_start) {
        scan_start -= 1;
    }
    let scan = &accumulated[scan_start..];

    // Longest suffix of the accumulated text that is a prefix of the incoming frame wins,
    // so the smallest safe delta is emitted. One reverse pass counts characters once;
    // work is bounded by the 64 KiB scan window.
    let mut suffix_chars = 0usize;
    for (byte_start, _) in scan.char_indices().rev() {
        suffix_chars += 1;
        if suffix_chars < DEDUP_MIN_OVERLAP_CHARS {
            continue;
        }
        let suffix = &scan[byte_start..];
        if suffix.len() > incoming.len() {
            // Longer suffixes can only get longer as we walk backwards.
            break;
        }
        if let Some(rest) = incoming.strip_prefix(suffix) {
            return DedupDecision::Emit(rest);
        }
    }
    // No >=12-char overlap in the tail. If the snapshot still resembles previously emitted
    // text (a revision or reorder), dropping it avoids re-emitting the whole string. A
    // genuinely new segment has no such resemblance and must still be delivered once.
    if snapshot_revises_emitted(accumulated, incoming) {
        tracing::warn!(
            channel_len = accumulated.len(),
            incoming_len = incoming.len(),
            incoming_hash = snapshot_hash(incoming),
            "dropping unmatched cumulative snapshot that revises already-emitted text"
        );
        return DedupDecision::Dropped;
    }
    DedupDecision::Emit(incoming)
}

/// Detect revisions: a long shared prefix that diverges near the end, or a long shared
/// suffix (frame reordered/prefix trimmed). Both mean the text was already substantially
/// emitted and re-sending the entire snapshot would duplicate it.
fn snapshot_revises_emitted(accumulated: &str, incoming: &str) -> bool {
    let common_prefix = accumulated
        .bytes()
        .zip(incoming.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let common_suffix = accumulated
        .as_bytes()
        .iter()
        .rev()
        .zip(incoming.as_bytes().iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    // Require a strong signal (at least 2/3 of the shorter frame AND >=12 bytes) so a
    // short shared boilerplate prefix cannot swallow a genuinely new segment.
    let shorter = accumulated.len().min(incoming.len());
    let threshold = (shorter * 2 / 3).max(DEDUP_MIN_OVERLAP_CHARS);
    if common_prefix >= threshold || common_suffix >= threshold {
        return true;
    }
    // A middle-edited or trimmed cumulative snapshot keeps both a >=12-byte prefix AND
    // a >=12-byte suffix that together cover at least 2/3 of the shorter frame.
    common_prefix >= DEDUP_MIN_OVERLAP_CHARS
        && common_suffix >= DEDUP_MIN_OVERLAP_CHARS
        && common_prefix.saturating_add(common_suffix).min(shorter) >= threshold
}

fn append_utf8_chunk(raw_bytes: &mut Vec<u8>, chunk: &[u8], out: &mut String) {
    if raw_bytes.is_empty()
        && let Ok(text) = std::str::from_utf8(chunk)
    {
        out.push_str(text);
        return;
    }
    raw_bytes.extend_from_slice(chunk);
    loop {
        match std::str::from_utf8(raw_bytes) {
            Ok(text) => {
                out.push_str(text);
                raw_bytes.clear();
                break;
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid > 0
                    && let Ok(prefix) = std::str::from_utf8(&raw_bytes[..valid])
                {
                    out.push_str(prefix);
                }
                if let Some(invalid_len) = err.error_len() {
                    out.push('\u{FFFD}');
                    raw_bytes.drain(..valid + invalid_len);
                } else {
                    raw_bytes.drain(..valid);
                    break;
                }
            }
        }
    }
}

fn snapshot_hash(text: &str) -> String {
    // Small, stable, bounded fingerprint for logs; not a security hash.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn pop_next_sse_frame(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n").map(|pos| (pos, 2usize));
    let crlf = buffer.find("\r\n\r\n").map(|pos| (pos, 4usize));
    let (pos, delim_len) = match (lf, crlf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let frame = buffer[..pos].to_owned();
    buffer.drain(..pos + delim_len);
    Some(frame)
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

    #[test]
    fn dedup_delta_emits_only_suffix_for_normal_prefix_extension() {
        assert_eq!(
            compute_dedup_delta("The quick brown fox", "The quick brown fox jumps over"),
            " jumps over"
        );
        assert_eq!(compute_dedup_delta("same", "same"), "");
        assert_eq!(compute_dedup_delta("", "first"), "first");
    }

    #[test]
    fn dedup_delta_merges_non_prefix_overlap_without_repeated_text() {
        let accumulated = "alpha beta gamma delta epsilon";
        let incoming = "beta gamma delta epsilon zeta";
        assert_eq!(compute_dedup_delta(accumulated, incoming), " zeta");

        let mut full = accumulated.to_owned();
        full.push_str(compute_dedup_delta(accumulated, incoming));
        assert_eq!(full, "alpha beta gamma delta epsilon zeta");
        assert_eq!(full.matches("beta gamma delta epsilon").count(), 1);
    }

    #[test]
    fn dedup_delta_does_not_duplicate_reordered_cumulative_frame() {
        let prefix = "第一段前言，这里是累积的中文文本，用于测试重叠边界";
        let accumulated = format!("{prefix}，后续新增内容");
        let incoming = format!("{prefix}，后续新增内容，再追加一段");
        let delta = compute_dedup_delta(&accumulated, &incoming);
        assert_eq!(delta, "，再追加一段");

        let mut full = accumulated.clone();
        full.push_str(delta);
        assert_eq!(full, incoming);
        assert_eq!(
            full.matches("这里是累积的中文文本，用于测试重叠边界")
                .count(),
            1
        );
    }

    #[test]
    fn dedup_delta_emits_whole_incoming_for_no_overlap_segment() {
        assert_eq!(
            compute_dedup_delta("alpha beta gamma", "completely separate segment"),
            "completely separate segment"
        );
        assert_eq!(compute_dedup_delta("alpha beta gamma", "short"), "short");
    }

    #[test]
    fn snapshot_normal_prefix_extension_emits_only_suffix() {
        let accumulated = "The quick brown fox jumps";
        let incoming = "The quick brown fox jumps over the lazy dog";
        assert_eq!(
            compute_dedup_delta(accumulated, incoming),
            " over the lazy dog"
        );
    }

    #[test]
    fn snapshot_reordered_cumulative_frame_does_not_duplicate_text() {
        let accumulated = "alpha beta gamma delta epsilon zeta";
        let incoming = "alpha beta gamma epsilon zeta";
        let delta = compute_dedup_delta(accumulated, incoming);
        assert_eq!(delta, "");
        assert_eq!(accumulated.matches("alpha beta gamma").count(), 1);
    }

    #[test]
    fn snapshot_revised_frame_does_not_reemit_full_text() {
        let accumulated = "The answer is 42 and here is more explanatory context text";
        let incoming = "The answer is 57 and here is more explanatory context text";
        assert_eq!(compute_dedup_delta(accumulated, incoming), "");
        assert_eq!(accumulated.matches("The answer is").count(), 1);
    }

    #[test]
    fn snapshot_genuinely_new_segment_is_emitted_once() {
        let accumulated = "The answer is 42 and here is more explanatory context text";
        let incoming = "Separate follow-up: please open config.rs and inspect line 42 now";
        let delta = compute_dedup_delta(accumulated, incoming);
        assert_eq!(delta, incoming);
        assert_eq!(
            format!("{accumulated}{delta}")
                .matches("Separate follow-up:")
                .count(),
            1
        );
    }

    #[test]
    fn tool_calls_with_distinct_provider_ids_are_preserved() {
        let mut decoder = CloudCodeSseDecoder::new(ModelFamily::Gemini, "s".to_owned(), None, 1000);
        let sse = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_alpha\",\"name\":\"read\",\"args\":{}}}]}}]}}\n\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_beta\",\"name\":\"read\",\"args\":{}}}]}}]}}\n\n"
        );
        let events = decoder.push_bytes(sse.as_bytes()).expect("push");
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_alpha", "call_beta"]);
    }

    #[test]
    fn tool_calls_without_ids_stay_distinct() {
        let mut decoder = CloudCodeSseDecoder::new(ModelFamily::Gemini, "s".to_owned(), None, 1000);
        let sse = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{}}}]}}]}}\n\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"\",\"name\":\"read\",\"args\":{}}}]}}]}}\n\n"
        );
        let events = decoder.push_bytes(sse.as_bytes()).expect("push");
        let ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert!(!ids[0].starts_with("tool_") && !ids[1].starts_with("tool_"));
    }

    #[test]
    fn thought_signature_mention_is_not_an_order_error() {
        assert!(!is_thinking_order_error(
            "quota exhausted: thought signature telemetry unavailable"
        ));
        assert!(!is_thinking_order_error(
            "invalid request: unsupported temperature value"
        ));
        assert!(is_thinking_order_error(
            "invalid request: functionCall is missing a thought_signature"
        ));
        assert!(is_thinking_order_error(
            "thinking_block_order: thought signature blocks must be ordered before text"
        ));
        assert!(is_thinking_order_error(
            "invalid signature in thinking block"
        ));
        assert!(is_thinking_order_error(
            "corrupted thought_signature in history"
        ));
    }

    #[test]
    fn snapshot_incremental_delta_is_not_dropped_by_subsequence_match() {
        let accumulated = "Please inspect src/main.rs, crates/oc-llm/src/client.rs, and tests/http.rs for streaming timeout details and error handling paths.";
        let incoming = ", and retry stream read.";
        assert_eq!(compute_dedup_delta(accumulated, incoming), incoming);
    }

    #[test]
    fn tool_call_emits_input_lifecycle_and_caches_real_signature() {
        let sig_cache = Arc::new(Mutex::new(
            SignatureCache::load(None, crate::config::SignatureCacheConfig::default(), 1000)
                .expect("cache"),
        ));
        let mut decoder = CloudCodeSseDecoder::new(
            ModelFamily::Gemini,
            "ses_tool_sig".to_owned(),
            Some(sig_cache.clone()),
            1000,
        );
        let sse = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"subagent\",\"args\":{\"agent\":\"explore\"}},\"thoughtSignature\":\"sig_real_function_call_token\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut events = decoder.push_bytes(sse.as_bytes()).expect("push");
        events.extend(decoder.finish().expect("finish"));

        let start_pos = events
            .iter()
            .position(|e| matches!(e, LlmEvent::ToolInputStart { id, name } if id == "call_1" && name == "subagent"))
            .expect("ToolInputStart emitted");
        let end_pos = events
            .iter()
            .position(|e| matches!(e, LlmEvent::ToolInputEnd { id, .. } if id == "call_1"))
            .expect("ToolInputEnd emitted");
        let call_pos = events
            .iter()
            .position(|e| matches!(e, LlmEvent::ToolCall { id, name, .. } if id == "call_1" && name == "subagent"))
            .expect("ToolCall emitted");
        assert!(start_pos < end_pos && end_pos < call_pos);

        let mut guard = sig_cache.lock().expect("lock");
        let mut contents = vec![serde_json::json!({
            "role": "model",
            "parts": [{
                "functionCall": {
                    "id": "call_1",
                    "name": "subagent",
                    "args": { "agent": "explore" }
                }
            }]
        })];
        crate::thinking::repair_contents_thinking(
            &mut contents,
            ModelFamily::Gemini,
            true,
            true,
            "ses_tool_sig",
            Some(&mut guard),
            1200,
        );
        assert_eq!(
            contents[0]["parts"][0]["thoughtSignature"],
            "sig_real_function_call_token"
        );
    }

    #[test]
    fn decodes_split_utf8_chunks_and_mixed_crlf_lf_frames() {
        let mut decoder =
            CloudCodeSseDecoder::new(ModelFamily::Gemini, "ses_utf8".to_owned(), None, 1000);
        let frame1 = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Merhaba düny\u{00e4} 🌍\"}]}}]}}\r\n\r\n";
        let frame2 = "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Merhaba düny\u{00e4} 🌍!\"}]},\"finishReason\":\"STOP\"}]}}\n\n";
        let combined = format!("{frame1}{frame2}data: [DONE]\n\n");
        let bytes = combined.as_bytes();
        // Split right inside the 4-byte emoji sequence (bytes near "🌍").
        let split_at = frame1.find("🌍").expect("emoji") + 2;
        let mut events = decoder.push_bytes(&bytes[..split_at]).expect("part 1");
        events.extend(decoder.push_bytes(&bytes[split_at..]).expect("part 2"));
        events.extend(decoder.finish().expect("finish"));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Merhaba düny\u{00e4} 🌍!");
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn prompt_block_reason_and_nested_response_error_surface_cleanly() {
        let mut blocked =
            CloudCodeSseDecoder::new(ModelFamily::Gemini, "ses_blk".to_owned(), None, 1000);
        blocked
            .push_bytes(b"data: {\"response\":{\"promptFeedback\":{\"blockReason\":\"PROHIBITED_CONTENT\",\"blockReasonMessage\":\"policy\"}}}\n\n")
            .expect("push");
        assert_eq!(
            blocked.finish(),
            Err(StreamDecodeFailure::Fatal(
                "Cloud Code blocked prompt (PROHIBITED_CONTENT: policy)".to_owned()
            ))
        );

        let mut nested =
            CloudCodeSseDecoder::new(ModelFamily::Gemini, "ses_nest".to_owned(), None, 1000);
        let err = nested
            .push_bytes(
                b"data: {\"response\":{\"error\":{\"message\":\"RESOURCE_EXHAUSTED: Quota exceeded for quota metric\"}}}\n\n",
            )
            .expect_err("nested error should fail");
        assert!(matches!(err, StreamDecodeFailure::QuotaExhausted(_)));
    }
}
