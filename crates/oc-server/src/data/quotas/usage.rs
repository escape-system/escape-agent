//! Local usage rollup: the session/message scan, its 60 s cache and the
//! per-provider window totals the quota response carries.

use std::collections::BTreeMap;
use std::sync::Arc;

use oc_store::Store;
use serde_json::Value;

use crate::AppState;
use crate::data::session_ext::common::{messages_all, sessions_all, time_of};
use crate::error::ErrorResponse;

use super::CACHE_TTL_MS;
use super::types::{ModelUsageRow, ProviderLocalUsage, ProviderUsageAccum};

const FIVE_HOURS_MS: i64 = 5 * 3_600_000;
const ONE_DAY_MS: i64 = 86_400_000;
const SEVEN_DAYS_MS: i64 = 7 * 86_400_000;
const THIRTY_DAYS_MS: i64 = 30 * 86_400_000;

/// In-process rollup of the session/message scan the UI polls every 60 s.
#[derive(Debug, Default)]
pub(super) struct UsageCache {
    computed_at_ms: i64,
    providers: BTreeMap<String, ProviderLocalUsage>,
}

impl UsageCache {
    fn is_fresh(&self, now_ms: i64) -> bool {
        self.computed_at_ms > 0 && now_ms.saturating_sub(self.computed_at_ms) < CACHE_TTL_MS
    }
}

/// Local usage per provider, from the 60 s cache unless a refresh was asked for.
pub(super) async fn usage_for(
    state: &AppState,
    now_ms: i64,
    force_refresh: bool,
) -> Result<BTreeMap<String, ProviderLocalUsage>, ErrorResponse> {
    if !force_refresh {
        let cache = state.quota_cache().read().await;
        if cache.usage.is_fresh(now_ms) {
            return Ok(cache.usage.providers.clone());
        }
    }
    let providers = scan_local_usage(state.store(), now_ms).await?;
    state.quota_cache().write().await.usage = UsageCache {
        computed_at_ms: now_ms,
        providers: providers.clone(),
    };
    Ok(providers)
}

/// Local usage from a fresh cache only: a stale or empty cache yields an empty
/// map, so a cache-first GET never scans the store either.
pub(super) async fn usage_cached(
    state: &AppState,
    now_ms: i64,
) -> BTreeMap<String, ProviderLocalUsage> {
    let cache = state.quota_cache().read().await;
    if cache.usage.is_fresh(now_ms) {
        cache.usage.providers.clone()
    } else {
        BTreeMap::new()
    }
}

async fn scan_local_usage(
    store: Option<&Arc<Store>>,
    now_ms: i64,
) -> Result<BTreeMap<String, ProviderLocalUsage>, ErrorResponse> {
    let Some(store) = store else {
        return Ok(BTreeMap::new());
    };
    let sessions = sessions_all(store, None).await?;
    let mut accums: BTreeMap<String, ProviderUsageAccum> = BTreeMap::new();
    let five_hours_since = now_ms - FIVE_HOURS_MS;
    let day_since = now_ms - ONE_DAY_MS;
    let week_since = now_ms - SEVEN_DAYS_MS;
    let month_since = now_ms - THIRTY_DAYS_MS;

    for session in &sessions {
        let Some(session_id) = session.get("id").and_then(Value::as_str) else {
            continue;
        };
        let created = session
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        for message in messages_all(store, session_id).await? {
            if message.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let ts = time_of(&message).unwrap_or(0);
            if ts < created {
                continue;
            }
            let Some(provider_id) = message
                .pointer("/model/providerID")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let entry = accums.entry(provider_id.to_owned()).or_default();
            entry.all_time.add(&message);
            if ts >= month_since {
                entry.month.add(&message);
            }
            if ts >= week_since {
                entry.week.add(&message);
            }
            if ts >= day_since {
                entry.day.add(&message);
            }
            if ts >= five_hours_since {
                entry.five_hours.add(&message);
            }
            if let Some(model_id) = message
                .pointer("/model/id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let tokens = message_token_total(&message);
                let cost = message
                    .get("cost")
                    .and_then(Value::as_f64)
                    .filter(|c| c.is_finite())
                    .unwrap_or(0.0);
                let model_entry = entry
                    .models
                    .entry(model_id.to_owned())
                    .or_insert((0, 0, 0.0));
                model_entry.0 += 1;
                model_entry.1 += tokens;
                model_entry.2 += cost;
            }
        }
    }

    Ok(accums
        .into_iter()
        .map(|(provider_id, acc)| (provider_id, finalize_provider_usage(acc)))
        .collect())
}

fn finalize_provider_usage(acc: ProviderUsageAccum) -> ProviderLocalUsage {
    let mut models: Vec<ModelUsageRow> = acc
        .models
        .into_iter()
        .map(
            |(model_id, (steps, total_tokens, cost_usd))| ModelUsageRow {
                model_id,
                steps,
                total_tokens,
                cost_usd,
            },
        )
        .collect();
    models.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| b.cost_usd.total_cmp(&a.cost_usd))
            .then_with(|| a.model_id.cmp(&b.model_id))
    });
    models.truncate(5);
    ProviderLocalUsage {
        five_hours: acc.five_hours,
        day: acc.day,
        week: acc.week,
        month: acc.month,
        all_time: acc.all_time,
        top_models: models,
    }
}

fn message_token_total(msg: &Value) -> i64 {
    ["input", "output", "reasoning", "cache/read", "cache/write"]
        .into_iter()
        .map(|field| {
            msg.pointer(&format!("/tokens/{field}"))
                .and_then(Value::as_i64)
                .unwrap_or(0)
        })
        .sum()
}
