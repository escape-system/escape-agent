//! Local usage rollup: the session/message scan, its 60 s cache and the
//! per-provider window totals the quota response carries.

use std::collections::BTreeMap;
use std::sync::Arc;

use oc_store::{Store, WindowSum};

use crate::AppState;
use crate::error::ErrorResponse;

use super::CACHE_TTL_MS;
use super::types::{ModelUsageRow, ProviderLocalUsage, ProviderUsageAccum, UsageWindowTotals};

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

fn add_window(target: &mut UsageWindowTotals, src: WindowSum) {
    target.steps += src.steps;
    target.input_tokens += src.input_tokens;
    target.output_tokens += src.output_tokens;
    target.reasoning_tokens += src.reasoning_tokens;
    target.cache_read_tokens += src.cache_read_tokens;
    target.cache_write_tokens += src.cache_write_tokens;
    target.total_tokens += src.input_tokens
        + src.output_tokens
        + src.reasoning_tokens
        + src.cache_read_tokens
        + src.cache_write_tokens;
    if src.cost_usd.is_finite() {
        target.cost_usd += src.cost_usd;
    }
}

async fn scan_local_usage(
    store: Option<&Arc<Store>>,
    now_ms: i64,
) -> Result<BTreeMap<String, ProviderLocalUsage>, ErrorResponse> {
    let Some(store) = store else {
        return Ok(BTreeMap::new());
    };
    let rows = store.local_provider_usage(now_ms).await?;
    let mut accums: BTreeMap<String, ProviderUsageAccum> = BTreeMap::new();
    for row in rows {
        let entry = accums.entry(row.provider_id).or_default();
        add_window(&mut entry.five_hours, row.five_hours);
        add_window(&mut entry.day, row.day);
        add_window(&mut entry.week, row.week);
        add_window(&mut entry.month, row.month);
        add_window(&mut entry.all_time, row.all_time);
        if !row.model_id.is_empty() && row.all_time.steps > 0 {
            let total_tokens = row.all_time.input_tokens
                + row.all_time.output_tokens
                + row.all_time.reasoning_tokens
                + row.all_time.cache_read_tokens
                + row.all_time.cache_write_tokens;
            let model_entry = entry.models.entry(row.model_id).or_insert((0, 0, 0.0));
            model_entry.0 += row.all_time.steps;
            model_entry.1 += total_tokens;
            if row.all_time.cost_usd.is_finite() {
                model_entry.2 += row.all_time.cost_usd;
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
