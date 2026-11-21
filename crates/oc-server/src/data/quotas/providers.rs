//! Provider entry construction and the live quota probe pass.

use std::collections::{BTreeMap, BTreeSet};

use oc_antigravity::ANTIGRAVITY_PROVIDER_ID;
use oc_config::ConfigService;
use oc_llm::{
    Catalog, ProviderQuotaProbeResult, ProviderQuotaTarget, ProviderRateLimit, QuotaCapability,
    fetch_provider_quotas, provider_quota_capability, quota::default_http_client,
};
use serde_json::Value;

use crate::AppState;

use super::CACHE_TTL_MS;
use super::antigravity::{AntigravityFetchData, build_antigravity_entry};
use super::credentials::{
    has_direct_google_api_key, resolve_connection_source, resolve_provider_api_key,
    resolve_provider_base_url,
};
use super::openrouter::resolve_openrouter_management_key;
use super::types::{ManagementKeySource, ProviderLocalUsage, ProviderQuotaEntry, QuotaWindowEntry};

/// Inputs that describe one probe pass rather than one provider.
pub(super) struct ProbePass<'a> {
    pub(super) available_ids: &'a BTreeSet<String>,
    pub(super) catalog: Option<&'a Catalog>,
    pub(super) config: Option<&'a ConfigService>,
    pub(super) credentials: &'a [Value],
    /// Re-probe even when the cached snapshot is still fresh.
    pub(super) force_refresh: bool,
    /// `false` means cache-first: never await a provider probe.
    pub(super) allow_probe: bool,
    pub(super) now_ms: i64,
}

pub(super) async fn probe_live_providers(
    state: &AppState,
    pass: ProbePass<'_>,
) -> BTreeMap<String, (ProviderQuotaProbeResult, bool)> {
    let ProbePass {
        available_ids,
        catalog,
        config,
        credentials,
        force_refresh,
        allow_probe,
        now_ms,
    } = pass;
    let mut results = BTreeMap::new();
    let mut to_probe = Vec::new();
    let openrouter_mgmt = resolve_openrouter_management_key(credentials, config).map(|key| key.key);

    {
        let cache = state.quota_cache().read().await;
        for id in available_ids {
            if id == ANTIGRAVITY_PROVIDER_ID {
                continue;
            }
            let (capability, _) = provider_quota_capability(id);
            if capability != QuotaCapability::LiveApi {
                continue;
            }
            let cached = cache.probes.get(id);
            if !force_refresh && let Some(cached) = cached {
                results.insert(id.clone(), (cached.clone(), true));
                let fresh = now_ms.saturating_sub(cached.fetched_at_ms) < CACHE_TTL_MS;
                if fresh || !allow_probe {
                    continue;
                }
            } else if !allow_probe {
                // Cache-first read: serve nothing rather than await a probe.
                continue;
            }
            if let Some(api_key) = resolve_provider_api_key(id, catalog, config, credentials) {
                let management_key = if id == "openrouter" {
                    openrouter_mgmt.clone()
                } else {
                    None
                };
                to_probe.push(ProviderQuotaTarget {
                    provider_id: id.clone(),
                    api_key,
                    management_key,
                    base_url: resolve_provider_base_url(id, catalog, config),
                });
            }
        }
    }

    if to_probe.is_empty() {
        return results;
    }

    let http = match state.llm() {
        Some(llm) => llm.http_client().clone(),
        None => default_http_client(),
    };
    let fetched = fetch_provider_quotas(&http, &to_probe).await;
    let mut cache = state.quota_cache().write().await;
    for probe in fetched {
        cache
            .probes
            .insert(probe.provider_id.clone(), probe.clone());
        results.insert(probe.provider_id.clone(), (probe, false));
    }
    results
}

pub(super) struct ProviderEntryInput<'a> {
    pub(super) provider_id: String,
    pub(super) connected: bool,
    pub(super) catalog: Option<&'a Catalog>,
    pub(super) config: Option<&'a ConfigService>,
    pub(super) credentials: &'a [Value],
    pub(super) ag_refresh: Option<&'a AntigravityFetchData>,
    pub(super) probe: Option<&'a (ProviderQuotaProbeResult, bool)>,
    pub(super) rate_limit: Option<ProviderRateLimit>,
    pub(super) usage: Option<ProviderLocalUsage>,
    pub(super) openrouter_mgmt: bool,
    pub(super) openrouter_source: Option<ManagementKeySource>,
}

pub(super) fn build_provider_entry(input: ProviderEntryInput<'_>) -> ProviderQuotaEntry {
    let provider_id = input.provider_id;
    let use_antigravity = provider_id == ANTIGRAVITY_PROVIDER_ID
        && (input.ag_refresh.is_some_and(|ag| !ag.accounts.is_empty())
            || !has_direct_google_api_key(input.catalog, input.config, input.credentials));

    if use_antigravity {
        return build_antigravity_entry(
            provider_id,
            "Google Antigravity".to_owned(),
            input.connected,
            input.ag_refresh,
            input.rate_limit,
            input.usage,
        );
    }

    let name = provider_display_name(&provider_id, input.catalog, input.config);
    let (capability, capability_note) = provider_quota_capability(&provider_id);
    let connection_source = resolve_connection_source(
        &provider_id,
        input.connected,
        input.catalog,
        input.config,
        input.credentials,
    );

    let has_management_key = (provider_id == "openrouter").then_some(input.openrouter_mgmt);
    let management_key_source = (provider_id == "openrouter")
        .then_some(input.openrouter_source)
        .flatten();

    if let Some((probe, cached)) = input.probe {
        let status = if probe.error.is_some() {
            "error".to_owned()
        } else if probe.status == "ok" && !probe.warnings.is_empty() {
            "partial".to_owned()
        } else {
            probe.status.clone()
        };
        return ProviderQuotaEntry {
            provider_id,
            name,
            connected: input.connected,
            connection_source,
            capability,
            capability_note: capability_note.map(str::to_owned),
            status,
            fetched_at_ms: Some(probe.fetched_at_ms),
            cached: Some(*cached),
            has_management_key,
            management_key_source,
            key_label: probe.key_label.clone(),
            rate_limit_rule: probe.rate_limit_rule.clone(),
            windows: probe
                .windows
                .iter()
                .map(QuotaWindowEntry::from_probe)
                .collect(),
            balances: probe.balances.clone(),
            spend_summary: probe.spend_summary.clone(),
            rate_limit: input.rate_limit,
            accounts: Vec::new(),
            keys: Vec::new(),
            local_usage: input.usage,
            warnings: probe.warnings.clone(),
            error: probe.error.clone(),
        };
    }

    let has_api_key =
        resolve_provider_api_key(&provider_id, input.catalog, input.config, input.credentials)
            .is_some();
    let status = unprobed_provider_status(
        input.connected,
        capability,
        has_api_key,
        input.rate_limit.is_some(),
    );

    ProviderQuotaEntry {
        provider_id,
        name,
        connected: input.connected,
        connection_source,
        capability,
        capability_note: capability_note.map(str::to_owned),
        status,
        fetched_at_ms: None,
        cached: None,
        has_management_key,
        management_key_source,
        key_label: None,
        rate_limit_rule: None,
        windows: Vec::new(),
        balances: Vec::new(),
        spend_summary: None,
        rate_limit: input.rate_limit,
        accounts: Vec::new(),
        keys: Vec::new(),
        local_usage: input.usage,
        warnings: Vec::new(),
        error: None,
    }
}

fn unprobed_provider_status(
    connected: bool,
    capability: QuotaCapability,
    has_api_key: bool,
    has_rate_limit: bool,
) -> String {
    if !connected {
        "disconnected"
    } else if capability == QuotaCapability::LiveApi && !has_api_key {
        "no_api_key"
    } else if has_rate_limit {
        "headers_only"
    } else {
        "local_only"
    }
    .to_owned()
}

fn provider_display_name(
    provider_id: &str,
    catalog: Option<&Catalog>,
    config: Option<&ConfigService>,
) -> String {
    if provider_id == ANTIGRAVITY_PROVIDER_ID {
        return "Google Antigravity".to_owned();
    }
    if let Some(name) = config
        .and_then(|cfg| {
            cfg.merged()
                .pointer(&format!("/providers/{provider_id}/name"))
        })
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return name.to_owned();
    }
    if let Some(provider) = catalog.and_then(|cat| cat.provider(provider_id))
        && !provider.name.is_empty()
    {
        return provider.name.clone();
    }
    provider_id.to_owned()
}
