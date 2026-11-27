//! Raw Cloud Code streaming: envelope construction, SSE decoder priming and lazy tail
//! stream assembly for one endpoint.

use crate::accounts::ModelFamily;
use crate::constants::{HeaderStyle, headers_for_style};
use crate::rotation::RateLimitReason;
use crate::service::{
    ActiveCredentials, AntigravityService, flush_signature_cache_handle_async, now_ms,
    spawn_flush_signature_cache_handle,
};
use crate::signature_cache::SignatureCache;
use crate::stream::{CloudCodeSseDecoder, StreamDecodeFailure};
use crate::transform::build_cloud_code_request;
use bytes::Bytes;
use futures_util::stream::{self, StreamExt};
use oc_llm::{LlmError, LlmEvent, LlmRequest, LlmStream};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use super::SingleAttempt;
use super::validation::{format_reqwest_error, try_single_endpoint};

type ByteStream =
    Pin<Box<dyn futures_util::Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>;

pub(super) async fn send_stream_once(
    svc: &AntigravityService,
    request: &LlmRequest,
    creds: &ActiveCredentials,
    style: HeaderStyle,
    override_style: Option<HeaderStyle>,
    force_strip_thinking: bool,
) -> SingleAttempt {
    let cfg = svc.config();
    let session_key = request
        .options
        .cache_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                request
                    .model
                    .model
                    .id
                    .split_once('#')
                    .map_or_else(|| request.model.model.id.clone(), |(a, _)| a.to_owned())
            },
            str::to_owned,
        );

    let transformed = {
        let sig_arc = svc.signature_cache();
        let mut sig_guard = sig_arc.lock().ok();
        build_cloud_code_request(
            request,
            &creds.project_id,
            &session_key,
            &cfg,
            override_style,
            sig_guard.as_deref_mut(),
            force_strip_thinking,
            now_ms(),
        )
    };

    let hub_version = svc.version().current();
    let header_set = headers_for_style(style, &hub_version);
    let mut last_failure: Option<SingleAttempt> = None;

    for base in svc.endpoints().for_style(style) {
        let attempt = try_single_endpoint(
            svc,
            base,
            &transformed.envelope,
            transformed.resolved.family,
            &session_key,
            creds,
            style,
            &header_set,
        )
        .await;
        match attempt {
            SingleAttempt::RetryableFailure { status, body } => {
                last_failure = Some(SingleAttempt::RetryableFailure { status, body });
            }
            other => return other,
        }
    }

    last_failure.unwrap_or_else(|| {
        SingleAttempt::Fatal(LlmError::Provider {
            message: "no upstream endpoint available for request".to_owned(),
        })
    })
}

pub(super) async fn prime_decoder_stream(
    resp: reqwest::Response,
    family: ModelFamily,
    session_key: String,
    sig_cache: Arc<StdMutex<SignatureCache>>,
) -> SingleAttempt {
    let mut decoder =
        CloudCodeSseDecoder::new(family, session_key, Some(Arc::clone(&sig_cache)), now_ms());
    let mut byte_stream: ByteStream = Box::pin(resp.bytes_stream());
    let mut initial_events: Vec<LlmEvent> = Vec::new();
    let mut eof = false;

    while initial_events.is_empty() {
        match byte_stream.next().await {
            Some(Ok(chunk)) => match decoder.push_bytes(&chunk) {
                Ok(evts) => initial_events.extend(evts),
                Err(failure) => {
                    tracing::debug!(
                        stage = "prime_decode",
                        cause = decode_failure_kind(&failure),
                        family = family.as_str(),
                        "antigravity: stream decode failure classified"
                    );
                    return in_band_or(failure);
                }
            },
            Some(Err(err)) => {
                return SingleAttempt::RetryableFailure {
                    status: 502,
                    body: format!("stream read error: {}", format_reqwest_error(&err)),
                };
            }
            None => {
                eof = true;
                match decoder.finish() {
                    Ok(evts) => {
                        initial_events.extend(evts);
                        flush_signature_cache_handle_async(&sig_cache, false).await;
                    }
                    Err(failure) => {
                        tracing::debug!(
                            stage = "prime_eof",
                            cause = decode_failure_kind(&failure),
                            family = family.as_str(),
                            "antigravity: stream decode failure classified"
                        );
                        return in_band_or(failure);
                    }
                }
                break;
            }
        }
    }

    SingleAttempt::Ready(build_llm_stream(
        initial_events,
        eof,
        byte_stream,
        decoder,
        sig_cache,
    ))
}

/// Stable log label for a decoder failure. Never includes the provider message.
fn decode_failure_kind(failure: &StreamDecodeFailure) -> &'static str {
    match failure {
        StreamDecodeFailure::QuotaExhausted(_) => "quota_exhausted",
        StreamDecodeFailure::RateLimited(_) => "rate_limited",
        StreamDecodeFailure::ThinkingBlockOrder(_) => "thinking_block_order",
        StreamDecodeFailure::EmptyStream => "empty_stream",
        StreamDecodeFailure::Fatal(_) => "fatal",
    }
}

/// In-band quota/rate-limit SSE frames arrive on an HTTP 200 stream, so they are reported as
/// `RateLimited { status: 200 }`: the pool then marks the serving account and rotates to the
/// next one instead of failing the turn with raw provider text. Frames without account
/// attribution stay fatal with their verbatim provider text.
fn in_band_or(failure: StreamDecodeFailure) -> SingleAttempt {
    match failure {
        StreamDecodeFailure::QuotaExhausted(msg) => SingleAttempt::RateLimited {
            reason: RateLimitReason::QuotaExhausted,
            retry_after_ms: None,
            status: 200,
            body: msg,
        },
        StreamDecodeFailure::RateLimited(msg) => SingleAttempt::RateLimited {
            reason: RateLimitReason::RateLimitExceeded,
            retry_after_ms: None,
            status: 200,
            body: msg,
        },
        StreamDecodeFailure::ThinkingBlockOrder(_) => SingleAttempt::RetryStripThinking,
        StreamDecodeFailure::EmptyStream => SingleAttempt::RetryEmptyResponse,
        StreamDecodeFailure::Fatal(msg) => {
            SingleAttempt::Fatal(LlmError::Provider { message: msg })
        }
    }
}

struct TailState {
    bytes: ByteStream,
    decoder: CloudCodeSseDecoder,
    sig_cache: Arc<StdMutex<SignatureCache>>,
    finished: bool,
}

impl Drop for TailState {
    fn drop(&mut self) {
        spawn_flush_signature_cache_handle(&self.sig_cache, false);
    }
}

/// Provider-facing text for a decoder failure observed *after* the first event was emitted.
///
/// Rotation is no longer possible at that point (the caller already owns a stream), so the
/// message must explain the account-level cause instead of echoing raw provider prose.
fn mid_stream_error_message(failure: StreamDecodeFailure) -> String {
    match failure {
        // The exact phrase required by the `EmptyResponse` classifier needle in
        // `oc_llm::error::MESSAGE_RULES`; a mid-stream empty tail must advance
        // the model fallback chain just like an exhausted pre-stream retry loop.
        StreamDecodeFailure::EmptyStream => "upstream stream ended unexpectedly".to_owned(),
        StreamDecodeFailure::QuotaExhausted(m) => {
            format!("Antigravity account quota exhausted mid-stream (reason: quota_exhausted): {m}")
        }
        StreamDecodeFailure::RateLimited(m) => {
            format!(
                "Antigravity account rate limited mid-stream (reason: rate_limit_exceeded): {m}"
            )
        }
        // History/thinking rejections and non-recoverable frames already carry provider text
        // that the caller must see verbatim.
        StreamDecodeFailure::ThinkingBlockOrder(m) | StreamDecodeFailure::Fatal(m) => m,
    }
}

fn build_llm_stream(
    initial_events: Vec<LlmEvent>,
    already_finished: bool,
    byte_stream: ByteStream,
    decoder: CloudCodeSseDecoder,
    sig_cache: Arc<StdMutex<SignatureCache>>,
) -> LlmStream {
    let head = stream::iter(initial_events);
    if already_finished {
        return Box::pin(head);
    }

    let tail = stream::unfold(
        TailState {
            bytes: byte_stream,
            decoder,
            sig_cache,
            finished: false,
        },
        |mut st| async move {
            if st.finished {
                return None;
            }
            loop {
                match st.bytes.next().await {
                    Some(Ok(chunk)) => match st.decoder.push_bytes(&chunk) {
                        Ok(evts) if evts.is_empty() => {}
                        Ok(evts) => return Some((stream::iter(evts), st)),
                        Err(err) => {
                            st.finished = true;
                            flush_signature_cache_handle_async(&st.sig_cache, false).await;
                            let msg = mid_stream_error_message(err);
                            return Some((
                                stream::iter(vec![LlmEvent::ProviderError { message: msg }]),
                                st,
                            ));
                        }
                    },
                    Some(Err(err)) => {
                        st.finished = true;
                        flush_signature_cache_handle_async(&st.sig_cache, false).await;
                        return Some((
                            stream::iter(vec![LlmEvent::ProviderError {
                                message: format!(
                                    "stream transport error: {}",
                                    format_reqwest_error(&err)
                                ),
                            }]),
                            st,
                        ));
                    }
                    None => {
                        st.finished = true;
                        let final_evts = match st.decoder.finish() {
                            Ok(evts) => evts,
                            Err(err) => {
                                vec![LlmEvent::ProviderError {
                                    message: mid_stream_error_message(err),
                                }]
                            }
                        };
                        flush_signature_cache_handle_async(&st.sig_cache, false).await;
                        return Some((stream::iter(final_evts), st));
                    }
                }
            }
        },
    )
    .flatten();

    Box::pin(head.chain(tail))
}
