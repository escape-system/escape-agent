//! Account-pool dispatch: per-style selection loops, token refunds, cooldown waits and
//! fallback-pool signalling for a single Cloud Code stream attempt.

use crate::accounts::ModelFamily;
use crate::config::SchedulingMode;
use crate::constants::HeaderStyle;
use crate::error::AntigravityError;
use crate::service::{AntigravityService, now_ms};
use oc_llm::{LlmError, LlmRequest};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::DispatchOutcome;
use super::single::handle_single_attempt;
use super::stream::send_stream_once;

pub(super) async fn dispatch_pool_stream(
    svc: &AntigravityService,
    request: &LlmRequest,
    style: HeaderStyle,
    override_style: Option<HeaderStyle>,
    force_strip_thinking: bool,
) -> std::result::Result<DispatchOutcome, LlmError> {
    let cfg = svc.config();
    let family = ModelFamily::from_model(&request.model.model.id);
    let started = Instant::now();
    // Cache-first trades latency for prompt-cache affinity, so it never waits longer
    // than `max_cache_first_wait_seconds` even when the global budget is larger.
    let wait_budget_secs = if cfg.scheduling_mode == SchedulingMode::CacheFirst {
        cfg.max_rate_limit_wait_seconds
            .min(cfg.max_cache_first_wait_seconds)
    } else {
        cfg.max_rate_limit_wait_seconds
    };
    let max_wait = Duration::from_secs(wait_budget_secs);
    let mut excluded_indices: HashSet<usize> = HashSet::new();
    let mut last_error: Option<LlmError> = None;

    loop {
        let excluded_vec: Vec<usize> = excluded_indices.iter().copied().collect();
        let selected = svc
            .select_account_for_request(family, style, &request.model.model.id, &excluded_vec)
            .map_err(|e| to_llm_error(&e))?;

        let Some(acct) = selected else {
            match wait_when_pool_exhausted(
                svc,
                family,
                style,
                &request.model.model.id,
                &excluded_indices,
                &mut last_error,
                started,
                max_wait,
                cfg.quota_fallback,
            )
            .await?
            {
                PoolExhaustedAction::RetryAfterSleep => {
                    excluded_indices.clear();
                    continue;
                }
                PoolExhaustedAction::TryFallbackPool => {
                    return Ok(DispatchOutcome::TryFallbackPool);
                }
            }
        };

        let creds = match svc.credentials_for_account(acct.index, false).await {
            Ok(c) => c,
            Err(err) => {
                svc.refund_token(acct.index);
                excluded_indices.insert(acct.index);
                last_error = Some(to_llm_error(&err));
                continue;
            }
        };

        if cfg.request_jitter_max_ms > 0 {
            let jitter = rand::random_range(0..=cfg.request_jitter_max_ms);
            if jitter > 0 {
                tokio::time::sleep(Duration::from_millis(jitter)).await;
            }
        }

        let attempt = send_stream_once(
            svc,
            request,
            &creds,
            style,
            override_style,
            force_strip_thinking,
        )
        .await;

        if let Some(outcome) = handle_single_attempt(
            svc,
            request,
            acct.index,
            family,
            style,
            override_style,
            force_strip_thinking,
            attempt,
            &mut excluded_indices,
            &mut last_error,
            started,
            max_wait,
        )
        .await?
        {
            return Ok(outcome);
        }
    }
}

enum PoolExhaustedAction {
    RetryAfterSleep,
    TryFallbackPool,
}

#[allow(clippy::too_many_arguments)]
async fn wait_when_pool_exhausted(
    svc: &AntigravityService,
    family: ModelFamily,
    style: HeaderStyle,
    model_id: &str,
    excluded_indices: &HashSet<usize>,
    last_error: &mut Option<LlmError>,
    started: Instant,
    max_wait: Duration,
    quota_fallback: bool,
) -> std::result::Result<PoolExhaustedAction, LlmError> {
    let wait_ms = svc
        .min_wait_ms(family, style, Some(model_id))
        .map_err(|e| to_llm_error(&e))?;
    let Some(wait_ms) = wait_ms else {
        return Err(last_error.take().unwrap_or_else(|| LlmError::Provider {
            message: "no usable Google Antigravity accounts configured; run `opencode auth login`"
                .to_owned(),
        }));
    };

    // Pool-state diagnostics stay in the log; the surfaced message must not leak the raw
    // per-account provider sentence when the real cause is "every account is cooling".
    if let Some(err) = last_error.as_ref() {
        tracing::debug!(error = %err, family = family.as_str(), "antigravity: pool exhausted");
    }

    if wait_ms <= 0 && !excluded_indices.is_empty() {
        if quota_fallback && family == ModelFamily::Gemini {
            return Ok(PoolExhaustedAction::TryFallbackPool);
        }
        return Err(LlmError::Provider {
            message: format!(
                "all Antigravity accounts are cooling down until {} (every eligible account already failed this request)",
                now_ms()
            ),
        });
    }

    let wait_dur = Duration::from_millis(u64::try_from(wait_ms.max(0)).unwrap_or(0));
    if wait_dur.is_zero() || started.elapsed() + wait_dur > max_wait {
        if quota_fallback && family == ModelFamily::Gemini {
            return Ok(PoolExhaustedAction::TryFallbackPool);
        }
        let until_ms = now_ms().saturating_add(wait_ms.max(0));
        return Err(LlmError::Provider {
            message: format!(
                "all Antigravity accounts are cooling down until {until_ms} (next reset in {}s, exceeds the per-turn wait budget)",
                wait_dur.as_secs()
            ),
        });
    }

    tokio::time::sleep(wait_dur).await;
    Ok(PoolExhaustedAction::RetryAfterSleep)
}

fn to_llm_error(err: &AntigravityError) -> LlmError {
    LlmError::Provider {
        message: err.to_string(),
    }
}
