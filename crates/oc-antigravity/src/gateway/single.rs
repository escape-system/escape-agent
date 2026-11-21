//! Single-attempt outcome bookkeeping: marks accounts healthy/limited, refunds the token
//! bucket and translates upstream failures into pool exclusions.

use crate::accounts::ModelFamily;
use crate::constants::HeaderStyle;
use crate::service::AntigravityService;
use oc_llm::{LlmError, LlmRequest};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::stream::send_stream_once;
use super::{DispatchOutcome, SingleAttempt};

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) async fn handle_single_attempt(
    svc: &AntigravityService,
    request: &LlmRequest,
    account_index: usize,
    family: ModelFamily,
    style: HeaderStyle,
    override_style: Option<HeaderStyle>,
    force_strip_thinking: bool,
    attempt: SingleAttempt,
    excluded_indices: &mut HashSet<usize>,
    empty_excluded: &mut HashSet<usize>,
    last_error: &mut Option<LlmError>,
    started: Instant,
    max_wait: Duration,
) -> std::result::Result<Option<DispatchOutcome>, LlmError> {
    match attempt {
        SingleAttempt::Ready(stream) => {
            svc.mark_success(account_index);
            return Ok(Some(DispatchOutcome::Stream(stream)));
        }
        SingleAttempt::RetryStripThinking => {
            svc.refund_token(account_index);
            tracing::warn!(
                account_index,
                family = family.as_str(),
                style = style.as_str(),
                outcome = "retry_strip_thinking",
                "antigravity: refunded token bucket for non-upstream retry"
            );
            return Ok(Some(DispatchOutcome::RetryStripThinking));
        }
        SingleAttempt::RetryEmptyResponse => {
            svc.refund_token(account_index);
            excluded_indices.insert(account_index);
            empty_excluded.insert(account_index);
            *last_error = Some(LlmError::Provider {
                message: format!("Cloud Code account {account_index} returned an empty response"),
            });
            tracing::warn!(
                account_index,
                family = family.as_str(),
                style = style.as_str(),
                outcome = "retry_empty_response",
                "antigravity: refunded token bucket and rotating away from empty account"
            );
        }
        SingleAttempt::Unauthorized => {
            if let Ok(fresh) = svc.credentials_for_account(account_index, true).await
                && let SingleAttempt::Ready(stream) = send_stream_once(
                    svc,
                    request,
                    &fresh,
                    style,
                    override_style,
                    force_strip_thinking,
                )
                .await
            {
                svc.mark_success(account_index);
                return Ok(Some(DispatchOutcome::Stream(stream)));
            }
            excluded_indices.insert(account_index);
            *last_error = Some(LlmError::Provider {
                message: "upstream rejected OAuth token (HTTP 401)".to_owned(),
            });
        }
        SingleAttempt::ValidationRequired { reason, url, body } => {
            svc.mark_verification_required(account_index, Some(reason), url.clone());
            excluded_indices.insert(account_index);
            *last_error = Some(LlmError::Provider {
                message: format!(
                    "Google account verification required (403 validation_required){}: {body}",
                    url.as_deref()
                        .map(|u| format!(" ({u})"))
                        .unwrap_or_default()
                ),
            });
        }
        SingleAttempt::RateLimited {
            reason,
            retry_after_ms,
            status,
            body,
        } => {
            let delay = svc.mark_rate_limited(
                account_index,
                family,
                style,
                Some(&request.model.model.id),
                reason,
                retry_after_ms,
            );
            excluded_indices.insert(account_index);
            // `status == 200` means the failure arrived in-band in an SSE error frame
            // (`{"error":{"message":…}}`), not as an HTTP status line; name it that way so
            // diagnostics never pretend upstream answered with a 200-rate-limit.
            *last_error = Some(LlmError::Provider {
                message: if status == 200 {
                    format!("Cloud Code in-band {reason:?}: {body}")
                } else {
                    format!("Cloud Code HTTP {status} ({reason:?}, cooldown {delay}ms): {body}")
                },
            });
            let switch_first = svc.config().switch_on_first_rate_limit;
            if !switch_first && svc.account_count() == 1 {
                let dur = Duration::from_millis(u64::try_from(delay.max(0)).unwrap_or(1000));
                if started.elapsed() + dur <= max_wait {
                    tokio::time::sleep(dur).await;
                    excluded_indices.clear();
                }
            }
        }
        SingleAttempt::RetryableFailure { status, body } => {
            let _ = svc.mark_failure(account_index);
            excluded_indices.insert(account_index);
            *last_error = Some(LlmError::Provider {
                message: format!("Cloud Code transient error (HTTP {status}): {body}"),
            });
        }
        SingleAttempt::Fatal(err) => return Err(err),
    }
    Ok(None)
}
