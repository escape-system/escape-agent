//! The `GET /api/quotas` assembly: provider set, probe pass, ordering and the
//! spend summary.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use oc_llm::quota::now_epoch_ms;
use oc_llm::{ProviderRateLimit, QuotaCapability};
use oc_types::now_millis;

use crate::AppState;
use crate::data::model::active_provider_ids_with_credentials;
use crate::error::ErrorResponse;

use super::antigravity::refresh_antigravity_if_needed;
use super::openrouter::resolve_openrouter_management_key;
use super::providers::{ProbePass, ProviderEntryInput, build_provider_entry, probe_live_providers};
use super::types::{ProviderQuotaEntry, UnifiedQuotaSummary, UnifiedQuotasResponse};
use super::usage::{usage_cached, usage_for};

/// Assemble the report.
///
/// `force_refresh` ignores the 60 s TTL. `allow_probe` is the cache-first
/// switch: when it is false nothing here touches the network — only cached
/// probes and a fresh local-usage cache are used, so a GET can never block on a
/// provider probe.
pub(super) async fn build_unified_quotas(
    state: &AppState,
    force_refresh: bool,
    allow_probe: bool,
) -> Result<UnifiedQuotasResponse, ErrorResponse> {
    let now_ms = now_millis().max(now_epoch_ms());
    let credentials = match state.store() {
        Some(store) => store.list_credentials().await?,
        None => Vec::new(),
    };
    let catalog_generation = state.catalog();
    let catalog = catalog_generation.as_deref();
    let config = state.config().map(Arc::as_ref);

    // Only providers with a genuinely active credential (or a usable
    // Antigravity account) belong in the report. Providers that merely have
    // historical local usage or a captured rate-limit snapshot do **not**: they
    // would render a card the user cannot act on. `available_provider_ids` is
    // intentionally not used here — it keeps inactive providers visible to the
    // model/provider routes.
    let active_ids = match catalog {
        Some(cat) => active_provider_ids_with_credentials(state, cat, &credentials),
        None => BTreeSet::new(),
    };

    let usage_map = if allow_probe {
        usage_for(state, now_ms, force_refresh).await?
    } else {
        usage_cached(state, now_ms).await
    };
    let mut rate_limit_map = collect_rate_limits(state);

    // Active providers only; local usage is attached to those that appear.
    let provider_ids: BTreeSet<String> = active_ids.clone();

    let ag_refresh = refresh_antigravity_if_needed(state, force_refresh).await;
    let probe_results = probe_live_providers(
        state,
        ProbePass {
            available_ids: &active_ids,
            catalog,
            config,
            credentials: &credentials,
            force_refresh,
            allow_probe,
            now_ms,
        },
    )
    .await;

    let openrouter_key = resolve_openrouter_management_key(&credentials, config);
    let openrouter_mgmt = openrouter_key.is_some();
    let openrouter_source = openrouter_key.as_ref().map(|key| key.source);
    let mut providers = Vec::with_capacity(provider_ids.len());
    for id in provider_ids {
        // Every id in this set came from `active_provider_ids`, so the entry is
        // connected by construction; nothing re-adds an inactive provider.
        let connected = active_ids.contains(&id);
        let usage = usage_map.get(&id).cloned();
        let rate_limit = rate_limit_map.remove(&id);
        let probe = probe_results.get(&id);
        let entry = build_provider_entry(ProviderEntryInput {
            provider_id: id,
            connected,
            catalog,
            config,
            credentials: &credentials,
            ag_refresh: ag_refresh.as_ref(),
            probe,
            rate_limit,
            usage,
            openrouter_mgmt,
            openrouter_source,
        });
        providers.push(entry);
    }

    attach_provider_keys(state, now_ms, &mut providers).await;

    providers.sort_by(|a, b| {
        provider_sort_rank(a)
            .cmp(&provider_sort_rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.provider_id.cmp(&b.provider_id))
    });

    let summary = summarize_providers(&providers);
    // Only a full pass makes the snapshot fresh; the cache-first GET reads this
    // stamp and never writes it.
    if allow_probe {
        state.quota_cache().write().await.computed_at_ms = now_ms;
    }
    Ok(UnifiedQuotasResponse {
        fetched_at_ms: now_ms,
        summary,
        providers,
        refreshing: false,
    })
}

fn provider_sort_rank(entry: &ProviderQuotaEntry) -> u8 {
    if !entry.connected {
        return 3;
    }
    match entry.capability {
        QuotaCapability::LiveApi if !entry.windows.is_empty() || !entry.balances.is_empty() => 0,
        QuotaCapability::LiveApi => 1,
        QuotaCapability::RateLimitHeadersOnly => 2,
    }
}

fn summarize_providers(providers: &[ProviderQuotaEntry]) -> UnifiedQuotaSummary {
    let mut live_quota_count = 0_usize;
    let mut five_hours_usd = 0.0_f64;
    let mut day_usd = 0.0_f64;
    let mut week_usd = 0.0_f64;
    let mut month_usd = 0.0_f64;

    for entry in providers {
        if entry.capability == QuotaCapability::LiveApi
            && (!entry.windows.is_empty() || !entry.balances.is_empty())
        {
            live_quota_count += 1;
        }
        if let Some(usage) = &entry.local_usage {
            five_hours_usd += usage.five_hours.cost_usd;
            day_usd += usage.day.cost_usd;
            week_usd += usage.week.cost_usd;
            month_usd += usage.month.cost_usd;
        }
    }

    UnifiedQuotaSummary {
        provider_count: providers.len(),
        live_quota_count,
        spend5h_usd: five_hours_usd,
        spend24h_usd: day_usd,
        spend7d_usd: week_usd,
        spend30d_usd: month_usd,
    }
}

fn collect_rate_limits(state: &AppState) -> BTreeMap<String, ProviderRateLimit> {
    let mut map = BTreeMap::new();
    if let Some(llm) = state.llm() {
        for item in llm.rate_limits() {
            map.insert(item.provider_id.clone(), item);
        }
    }
    map
}

/// Join every provider's stored credentials with its per-key rotation state.
///
/// Order is the store's rotation order (active first, then ids ascending), so
/// `keys[0]` is the key a request would try first. Providers that own no
/// credential row keep an empty list, which the wire omits entirely.
///
/// `usedCostUsd` / `usedRequests` stay `0.0`/`0`: the usage rollup keys on
/// `model.providerID` only (`quotas/usage.rs`), so a per-key split does not
/// exist. Reporting a share of the provider total here would be invented data.
async fn attach_provider_keys(
    state: &AppState,
    now_ms: i64,
    providers: &mut [super::types::ProviderQuotaEntry],
) {
    let Some(store) = state.store() else {
        return;
    };
    // `KeyPool` normalizes provider ids (trim + lowercase); compare the same way.
    let health = state
        .key_pool()
        .map(|pool| pool.entries(now_ms))
        .unwrap_or_default();
    for entry in providers.iter_mut() {
        let provider_key = entry.provider_id.trim().to_ascii_lowercase();
        let rows = match store.credential_pool(&entry.provider_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, provider = %entry.provider_id, "could not read credential pool");
                continue;
            }
        };
        entry.keys = rows
            .into_iter()
            .map(|row| {
                let cooling = health.iter().find(|item| {
                    item.provider == provider_key
                        && item.credential_id.as_deref() == Some(row.id.as_str())
                });
                super::types::ProviderKeyEntry {
                    credential_id: row.id,
                    label: row.label,
                    active: row.active,
                    limited: cooling.is_some(),
                    cooldown_until_ms: cooling.map(|item| item.until_ms),
                    kind: cooling.map(|item| item.kind.as_str().to_owned()),
                    failures: cooling.map(|item| item.failures),
                    // No per-key usage attribution exists yet: the rollup is
                    // keyed by provider only. Zero, not a guessed split.
                    used_cost_usd: 0.0,
                    used_requests: 0,
                }
            })
            .collect();
    }
}
