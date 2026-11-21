//! Upstream response classification: validation gates, rate-limit parsing, thinking-order
//! recovery and empty-stream detection.

use crate::accounts::ModelFamily;
use crate::constants::HeaderStyle;
use crate::fingerprint::fingerprint_headers;
use crate::rotation::{extract_retry_info_from_body, parse_rate_limit_reason};
use crate::service::{ActiveCredentials, AntigravityService};
use crate::stream::is_thinking_order_error;
use oc_llm::LlmError;
use serde_json::Value;

use super::SingleAttempt;
use super::stream::prime_decoder_stream;

use std::error::Error as _;

pub(super) fn format_reqwest_error(err: &reqwest::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let msg = cause.to_string();
        if !msg.is_empty() && !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = cause.source();
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn try_single_endpoint(
    svc: &AntigravityService,
    base: &str,
    envelope: &Value,
    family: ModelFamily,
    session_key: &str,
    creds: &ActiveCredentials,
    style: HeaderStyle,
    header_set: &crate::constants::HeaderSet,
) -> SingleAttempt {
    let url = format!("{base}/v1internal:streamGenerateContent?alt=sse");
    let mut req_builder = svc
        .http_client()
        .post(&url)
        .bearer_auth(&creds.access_token)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("Accept-Encoding", "identity")
        .json(envelope);

    for (k, v) in header_set.to_pairs() {
        req_builder = req_builder.header(k, v);
    }
    if style == HeaderStyle::GeminiCli
        && let Some(ref fp) = creds.account.fingerprint
    {
        for (k, v) in fingerprint_headers(fp) {
            if matches!(k, "X-Goog-QuotaUser" | "X-Client-Device-Id") {
                req_builder = req_builder.header(k, v);
            }
        }
    }

    let resp = match req_builder.send().await {
        Ok(r) => r,
        Err(err) => {
            return SingleAttempt::RetryableFailure {
                status: 503,
                body: format_reqwest_error(&err),
            };
        }
    };

    let status = resp.status().as_u16();
    if let Some(mapped) = classify_non_success_response(status, resp.headers()) {
        return match mapped {
            ResponseCheck::ReadBody(kind) => {
                let body = resp.text().await.unwrap_or_default();
                finalize_error_attempt(kind, status, body)
            }
            ResponseCheck::Unauthorized => SingleAttempt::Unauthorized,
        };
    }

    prime_decoder_stream(resp, family, session_key.to_owned(), svc.signature_cache()).await
}

enum ResponseCheck {
    Unauthorized,
    ReadBody(ErrorKind),
}

enum ErrorKind {
    Forbidden,
    RateLimited {
        retry_after_hdr: Option<String>,
        retry_after_ms_hdr: Option<String>,
    },
    HttpError,
}

fn classify_non_success_response(
    status: u16,
    headers: &reqwest::header::HeaderMap,
) -> Option<ResponseCheck> {
    if (200..300).contains(&status) {
        return None;
    }
    if status == 401 {
        return Some(ResponseCheck::Unauthorized);
    }
    if status == 403 {
        return Some(ResponseCheck::ReadBody(ErrorKind::Forbidden));
    }
    if matches!(status, 429 | 503 | 529) {
        let retry_after_hdr = headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let retry_after_ms_hdr = headers
            .get("retry-after-ms")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        return Some(ResponseCheck::ReadBody(ErrorKind::RateLimited {
            retry_after_hdr,
            retry_after_ms_hdr,
        }));
    }
    Some(ResponseCheck::ReadBody(ErrorKind::HttpError))
}

fn finalize_error_attempt(kind: ErrorKind, status: u16, body: String) -> SingleAttempt {
    match kind {
        ErrorKind::Forbidden => {
            if is_validation_required_body(&body) {
                SingleAttempt::ValidationRequired {
                    reason: "validation_required".to_owned(),
                    url: extract_validation_url(&body),
                    body,
                }
            } else if is_account_suspended_or_tos_body(&body) {
                SingleAttempt::ValidationRequired {
                    reason: "tos_or_account_disabled".to_owned(),
                    url: extract_validation_url(&body),
                    body,
                }
            } else {
                SingleAttempt::Fatal(LlmError::Provider {
                    message: format!("Cloud Code HTTP 403 Forbidden: {body}"),
                })
            }
        }
        ErrorKind::RateLimited {
            retry_after_hdr,
            retry_after_ms_hdr,
        } => {
            let reason = parse_rate_limit_reason(status, &body);
            let delay = extract_retry_info_from_body(
                &body,
                retry_after_hdr.as_deref(),
                retry_after_ms_hdr.as_deref(),
            );
            SingleAttempt::RateLimited {
                reason,
                retry_after_ms: delay,
                status,
                body,
            }
        }
        ErrorKind::HttpError => {
            if status == 400 && is_thinking_order_error(&body) {
                SingleAttempt::RetryStripThinking
            } else if status == 404 || status >= 500 {
                SingleAttempt::RetryableFailure { status, body }
            } else {
                SingleAttempt::Fatal(LlmError::Provider {
                    message: format!("Cloud Code HTTP {status}: {body}"),
                })
            }
        }
    }
}

fn is_validation_required_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("validation_required")
        || lower.contains("verify your account")
        || (lower.contains("validation") && lower.contains("accounts.google.com"))
}

fn is_account_suspended_or_tos_body(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("terms of service")
        || lower.contains("tos violation")
        || lower.contains("account has been disabled")
        || lower.contains("account is suspended")
        || lower.contains("account_disabled")
}

fn extract_validation_url(body: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<Value>(body)
        && let Some(details) = json.pointer("/error/details").and_then(Value::as_array)
    {
        for detail in details {
            if let Some(url) = detail
                .get("metadata")
                .and_then(|m| m.get("validation_url"))
                .and_then(Value::as_str)
            {
                return Some(url.to_owned());
            }
            if let Some(links) = detail.get("links").and_then(Value::as_array) {
                for link in links {
                    if let Some(url) = link.get("url").and_then(Value::as_str) {
                        return Some(url.to_owned());
                    }
                }
            }
        }
    }
    for token in body.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ')' | '.'));
        if trimmed.starts_with("https://accounts.google.com/")
            || trimmed.starts_with("https://support.google.com/")
        {
            return Some(trimmed.to_owned());
        }
    }
    None
}
