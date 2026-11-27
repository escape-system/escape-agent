//! Key health: the `key_health` tool source and the `<key-health>` prompt line.
//!
//! Rotation is silent by design (`docs/27-key-pool.md`): a key that ran out of
//! quota is skipped on the *same* model without publishing
//! `session.model.fallback`. This module gives the model two ways to see that
//! state — the read-only `key_health` tool on demand, and a `<key-health>`
//! system-prompt line that is appended only while at least one key is cooling
//! down. With every key healthy neither surface invents an entry: the tool
//! still reports the layout, and the prompt block is absent so the cached
//! prefix is not churned.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use oc_llm::{Activation, KeyHealthEntry, KeyPool, ModelInfo, ModelStatus, Protocol};
use oc_store::{CredentialRow, Store, StoreError};
use oc_tools::key_health::{
    KeyHealthKey, KeyHealthModel, KeyHealthProvider, KeyHealthReport, KeyHealthSource,
};

use crate::runtime::Runtime;
use crate::subagent::settings::model_key;

/// Longest `<key-health>` line that is injected; an aggregate that would exceed
/// it falls back to the short form so the system prompt stays compact.
pub(crate) const MAX_BLOCK_CHARS: usize = 300;

/// Credential state of one runtime, bound to `key_health` for a single call.
#[derive(Debug)]
struct RuntimeKeyHealth {
    runtime: Runtime,
}

#[async_trait]
impl KeyHealthSource for RuntimeKeyHealth {
    async fn providers(
        &self,
        provider: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<KeyHealthProvider>, String> {
        let runtime = &self.runtime;
        let env = |id: &str| env_key_present(runtime, id);
        collect_key_health(
            &runtime.inner.config.store,
            runtime.key_pool(),
            &env,
            provider,
            now_ms,
        )
        .await
        .map_err(|error| format!("could not read the credential pool: {error}"))
    }

    async fn report(
        &self,
        provider: Option<&str>,
        include_models: bool,
        now_ms: i64,
    ) -> Result<KeyHealthReport, String> {
        let providers = self.providers(provider, now_ms).await?;
        if !include_models {
            return Ok(KeyHealthReport {
                providers,
                provider_models: BTreeMap::new(),
                agent_models: BTreeMap::new(),
            });
        }
        let (provider_models, agent_models) =
            collect_model_inventory(&self.runtime, &providers, now_ms).await;
        Ok(KeyHealthReport {
            providers,
            provider_models,
            agent_models,
        })
    }
}

fn normalize_model_ref(reference: &str) -> String {
    model_key(reference)
        .trim()
        .to_ascii_lowercase()
        .replace('.', "-")
}

fn hidden_model_refs(models_section: &serde_json::Value) -> HashSet<String> {
    let mut hidden = HashSet::new();
    match models_section.get("hidden") {
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                if let Some(reference) = item.as_str() {
                    hidden.insert(normalize_model_ref(reference));
                }
            }
        }
        Some(serde_json::Value::Object(map)) => {
            for (reference, flag) in map {
                if flag.as_bool() == Some(true) {
                    hidden.insert(normalize_model_ref(reference));
                }
            }
        }
        _ => {}
    }
    hidden
}

fn favorite_model_refs(models_section: &serde_json::Value) -> HashSet<String> {
    let mut favorites = HashSet::new();
    match models_section.get("favorites") {
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                if let Some(reference) = item.as_str() {
                    favorites.insert(normalize_model_ref(reference));
                }
            }
        }
        Some(serde_json::Value::Object(map)) => {
            for (reference, flag) in map {
                if flag.as_bool() == Some(true) {
                    favorites.insert(normalize_model_ref(reference));
                }
            }
        }
        _ => {}
    }
    favorites
}

fn model_notes_map(models_section: &serde_json::Value) -> BTreeMap<String, String> {
    let mut notes = BTreeMap::new();
    if let Some(map) = models_section
        .get("notes")
        .and_then(serde_json::Value::as_object)
    {
        for (reference, note) in map {
            if let Some(text) = note.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                notes.insert(normalize_model_ref(reference), text.to_owned());
            }
        }
    }
    notes
}

fn has_direct_google_access(runtime: &Runtime, credentials: &[serde_json::Value]) -> bool {
    if runtime.inner.config.client.hook().is_none() {
        return true;
    }
    if runtime
        .inner
        .config
        .config
        .merged()
        .get("providers")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|p| p.contains_key("google"))
    {
        return true;
    }
    if credentials.iter().any(|entry| {
        entry
            .get("integrationID")
            .and_then(serde_json::Value::as_str)
            == Some("google")
            && entry.get("active").and_then(serde_json::Value::as_bool) == Some(true)
            && entry
                .pointer("/value/type")
                .and_then(serde_json::Value::as_str)
                != Some("oauth")
    }) {
        return true;
    }
    env_key_present(runtime, "google")
}

fn merged_catalog_models(runtime: &Runtime, credentials: &[serde_json::Value]) -> Vec<ModelInfo> {
    let dynamic = runtime.inner.config.client.dynamic_models();
    let has_ag_google = dynamic
        .iter()
        .any(|m| same_provider(&m.provider_id, "google"));
    let direct_google = !has_ag_google || has_direct_google_access(runtime, credentials);
    let mut all = runtime.inner.config.catalog.current().models();
    if !direct_google {
        all.retain(|m| !same_provider(&m.provider_id, "google"));
    }
    for extra in dynamic {
        if let Some(pos) = all
            .iter()
            .position(|m| m.provider_id == extra.provider_id && m.id == extra.id)
        {
            all[pos] = extra;
        } else {
            all.push(extra);
        }
    }
    all
}

fn model_remaining_percent(
    quota: Option<&crate::subagent::settings::QuotaPolicy>,
    usage: Option<&[oc_store::ModelUsageRow]>,
    reference: &str,
    norm: &str,
) -> Option<f64> {
    let (Some(quota), Some(rows)) = (quota, usage) else {
        return None;
    };
    let budget = quota.budget_for(reference)?;
    let max = budget.max_cost_usd?;
    if max <= 0.0 {
        return Some(0.0);
    }
    let spend = rows
        .iter()
        .find(|r| norm == normalize_model_ref(&format!("{}/{}", r.provider_id, r.model_id)))
        .map_or(0.0, |r| r.cost);
    Some((((max - spend) / max) * 100.0).clamp(0.0, 100.0))
}

fn collect_agent_models(
    config: &oc_config::ConfigService,
    settings: &crate::subagent::settings::SubagentSettings,
) -> BTreeMap<String, String> {
    let defaults = crate::subagent::settings::model_defaults(config, settings);
    let mut agent_models = BTreeMap::new();
    for id in [
        "engineer",
        "organizer",
        "builder",
        "general",
        "explore",
        "ui",
        "docs",
    ] {
        if let Some(reference) = defaults.agent_models.get(id) {
            agent_models.insert(id.to_owned(), reference.clone());
        } else if let Some(reference) = defaults.policy_models.get(id) {
            agent_models.insert(id.to_owned(), reference.clone());
        } else if id == "organizer"
            && let Some(reference) = settings
                .models
                .chains
                .get("organizer")
                .and_then(|c| c.iter().map(|s| s.trim()).find(|s| !s.is_empty()))
        {
            agent_models.insert(id.to_owned(), reference.to_owned());
        }
    }
    if let Some(default_model) = defaults.default_model {
        agent_models.insert("default".to_owned(), default_model);
    }
    agent_models
}

fn is_selectable_model(model: &ModelInfo, catalog: &oc_llm::Catalog) -> bool {
    let is_chat = model.kind.as_deref().is_none_or(|kind| kind == "chat");
    is_chat
        && model.enabled
        && model.protocol != Protocol::Unsupported
        && model.status != ModelStatus::Deprecated
        && model.capabilities.tools
        && catalog
            .provider(&model.provider_id)
            .is_none_or(|p| p.activation != Activation::Disabled)
}

fn sort_provider_models(
    by_provider: &mut BTreeMap<String, Vec<KeyHealthModel>>,
    chain_order: &BTreeMap<String, usize>,
) {
    for models in by_provider.values_mut() {
        models.sort_by(|left, right| {
            let left_rank = chain_order
                .get(&normalize_model_ref(&left.reference))
                .copied()
                .unwrap_or(usize::MAX);
            let right_rank = chain_order
                .get(&normalize_model_ref(&right.reference))
                .copied()
                .unwrap_or(usize::MAX);
            right
                .favorite
                .cmp(&left.favorite)
                .then_with(|| left_rank.cmp(&right_rank))
                .then_with(|| left.model_id.cmp(&right.model_id))
        });
    }
}

async fn collect_model_inventory(
    runtime: &Runtime,
    providers: &[KeyHealthProvider],
    now_ms: i64,
) -> (
    BTreeMap<String, Vec<KeyHealthModel>>,
    BTreeMap<String, String>,
) {
    let config = &runtime.inner.config.config;
    let catalog = runtime.inner.config.catalog.current();
    let settings = crate::subagent::load_settings(config);
    let models_section = config.models_section();
    let hidden = hidden_model_refs(&models_section);
    let favorites = favorite_model_refs(&models_section);
    let notes = model_notes_map(&models_section);
    let chain_order: BTreeMap<String, usize> = settings
        .models
        .chain
        .iter()
        .enumerate()
        .map(|(idx, reference)| (normalize_model_ref(reference), idx))
        .collect();
    let credentials = runtime
        .inner
        .config
        .store
        .list_credentials()
        .await
        .unwrap_or_default();
    let unavail_map: BTreeMap<String, _> =
        runtime.model_health().entries(now_ms).into_iter().collect();
    let usage = match &settings.models.quota {
        Some(quota) => {
            let window_ms = i64::from(quota.window_hours) * 3_600_000;
            runtime
                .inner
                .config
                .store
                .model_usage(now_ms.saturating_sub(window_ms))
                .await
                .ok()
        }
        None => None,
    };

    let mut by_provider: BTreeMap<String, Vec<KeyHealthModel>> = BTreeMap::new();
    for model in merged_catalog_models(runtime, &credentials) {
        if !is_selectable_model(&model, &catalog) {
            continue;
        }
        let Some(matched_provider) = providers
            .iter()
            .find(|p| same_provider(&p.provider_id, &model.provider_id))
            .map(|p| p.provider_id.clone())
        else {
            continue;
        };
        let reference = format!("{}/{}", model.provider_id, model.id);
        let norm = normalize_model_ref(&reference);
        if hidden.contains(&norm) {
            continue;
        }
        let unavail = unavail_map.get(&norm);
        let remaining_pct = model_remaining_percent(
            settings.models.quota.as_ref(),
            usage.as_deref(),
            &reference,
            &norm,
        );
        let first_cost = model.cost.first();
        by_provider
            .entry(matched_provider)
            .or_default()
            .push(KeyHealthModel {
                model_id: model.id.clone(),
                reference,
                name: model.name.clone(),
                context_limit: Some(model.limit.context),
                output_limit: Some(model.limit.output),
                tools: model.capabilities.tools,
                variants: model.variants.iter().map(|v| v.id.clone()).collect(),
                cost_input_usd: first_cost.map(|c| c.input),
                cost_output_usd: first_cost.map(|c| c.output),
                favorite: favorites.contains(&norm),
                note: notes.get(&norm).cloned(),
                unavailable_kind: unavail.map(|u| u.kind.as_str().to_owned()),
                unavailable_until_ms: unavail.map(|u| u.until_ms),
                remaining_percent: remaining_pct,
                quota_limited: remaining_pct.map(|pct| pct <= 0.0),
            });
    }
    sort_provider_models(&mut by_provider, &chain_order);

    (by_provider, collect_agent_models(config, &settings))
}

impl Runtime {
    /// The `key_health` data source for this runtime.
    ///
    /// The tool lives in `oc-tools` and owns no state; `turn/run.rs` binds this
    /// source in task-local storage around `Tool::execute`, so every session of
    /// one process reports the same process-wide pool.
    #[must_use]
    pub(crate) fn key_health_source(&self) -> Arc<dyn KeyHealthSource> {
        Arc::new(RuntimeKeyHealth {
            runtime: self.clone(),
        })
    }
}

/// Join the stored credential pool with the in-memory cooldowns.
///
/// `env` reports whether a provider's environment variable is set; that key is
/// listed with `source: "env"` and no `credentialID` because it owns no row.
/// Providers are the union of the stored integrations and the providers holding
/// a cooldown, sorted by id; an explicit filter replaces that union with the
/// requested id. Keys keep the store's rotation order (active first, ids
/// ascending) with the environment slot last. A provider with no key at all is
/// dropped, so an unknown filter yields an empty list rather than a fake entry.
pub(crate) async fn collect_key_health(
    store: &Store,
    pool: &KeyPool,
    env: &(dyn Fn(&str) -> bool + Sync),
    filter: Option<&str>,
    now_ms: i64,
) -> Result<Vec<KeyHealthProvider>, StoreError> {
    let cooling = pool.entries(now_ms);
    let mut providers: Vec<String> = Vec::new();
    if let Some(id) = filter {
        providers.push(id.trim().to_owned());
    } else {
        for entry in store.list_credentials().await? {
            let Some(id) = entry
                .get("integrationID")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if !providers.iter().any(|known| same_provider(known, id)) {
                providers.push(id.to_owned());
            }
        }
        for entry in &cooling {
            if !providers
                .iter()
                .any(|known| same_provider(known, &entry.provider))
            {
                providers.push(entry.provider.clone());
            }
        }
    }

    let mut result = Vec::new();
    for requested in providers {
        let mut keys = Vec::new();
        for row in store.credential_pool(&requested).await? {
            keys.push(stored_key(&row, &cooling, &requested));
        }
        if env(&requested) {
            keys.push(env_key(&cooling, &requested));
        }
        if keys.is_empty() {
            continue;
        }
        result.push(KeyHealthProvider {
            provider_id: requested,
            keys,
        });
    }
    result.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    Ok(result)
}

fn stored_key(row: &CredentialRow, cooling: &[KeyHealthEntry], provider: &str) -> KeyHealthKey {
    let entry = cooling.iter().find(|entry| {
        same_provider(&entry.provider, provider)
            && entry.credential_id.as_deref() == Some(row.id.as_str())
    });
    KeyHealthKey {
        credential_id: Some(row.id.clone()),
        label: Some(row.label.clone()),
        active: row.active,
        limited: entry.is_some(),
        cooldown_until_ms: entry.map(|entry| entry.until_ms),
        kind: entry.map(|entry| entry.kind.as_str().to_owned()),
        failures: entry.map(|entry| entry.failures),
        source: None,
    }
}

fn env_key(cooling: &[KeyHealthEntry], provider: &str) -> KeyHealthKey {
    let entry = cooling
        .iter()
        .find(|entry| same_provider(&entry.provider, provider) && entry.credential_id.is_none());
    KeyHealthKey {
        credential_id: None,
        label: None,
        active: false,
        limited: entry.is_some(),
        cooldown_until_ms: entry.map(|entry| entry.until_ms),
        kind: entry.map(|entry| entry.kind.as_str().to_owned()),
        failures: entry.map(|entry| entry.failures),
        source: Some("env".to_owned()),
    }
}

fn same_provider(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

/// Whether the provider's environment credential is present, resolved exactly
/// like the rotation pool: the catalog's `env` list, first non-empty variable.
fn env_key_present(runtime: &Runtime, provider: &str) -> bool {
    runtime
        .inner
        .config
        .catalog
        .provider(provider)
        .is_some_and(|info| {
            info.env
                .iter()
                .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        })
}

/// The `<key-health>` system-prompt part for this runtime.
///
/// `None` while every key is healthy: a healthy pool needs no prompt line, and
/// an always-present block would churn the prompt-cache prefix on every
/// rotation. While a key is cooling, the line names the provider, the class,
/// the deadline and how many keys remain usable, and asks the model to say
/// which key it switched to.
pub(crate) async fn key_health_block(runtime: &Runtime) -> Option<String> {
    let now_ms = oc_types::now_millis();
    let cooling = runtime.key_pool().entries(now_ms);
    if cooling.is_empty() {
        return None;
    }
    let mut totals: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &cooling {
        let provider = entry.provider.trim().to_ascii_lowercase();
        if totals.contains_key(&provider) {
            continue;
        }
        let stored = runtime
            .inner
            .config
            .store
            .credential_pool(&provider)
            .await
            .map_or(0, |rows| rows.len());
        let env = usize::from(env_key_present(runtime, &provider));
        totals.insert(provider, stored + env);
    }
    block_from(&cooling, &totals)
}

/// Build the injected line from a cooldown snapshot and the per-provider key
/// totals. Pure, so the wording and the absence case are testable without a
/// runtime.
pub(crate) fn block_from(
    cooling: &[KeyHealthEntry],
    totals: &BTreeMap<String, usize>,
) -> Option<String> {
    let mut grouped: BTreeMap<String, Vec<&KeyHealthEntry>> = BTreeMap::new();
    for entry in cooling {
        grouped
            .entry(entry.provider.trim().to_ascii_lowercase())
            .or_default()
            .push(entry);
    }
    let mut iter = grouped.iter();
    let (provider, entries) = iter.next()?;
    let count = entries.len();
    let total = totals.get(provider).copied().unwrap_or(count);
    let usable = total.saturating_sub(count);
    let until = entries
        .iter()
        .map(|entry| entry.until_ms)
        .min()
        .unwrap_or(0);
    let kind = entries[0].kind.as_str();
    let mut text = format!(
        "<key-health>{count} {keys} for {provider} {verb} cooling down ({kind}) until {clock}; \
         {usable} {usable_keys} {usable_verb} usable. Say which key you switched to when it happens.",
        keys = key_word(count),
        verb = if count == 1 { "is" } else { "are" },
        clock = clock(until),
        usable_keys = key_word(usable),
        usable_verb = if usable == 1 { "remains" } else { "remain" },
    );
    if grouped.len() > 1 {
        let _ = write!(
            text,
            " {} other provider(s) also have cooling keys.",
            grouped.len() - 1
        );
    }
    text.push_str("</key-health>");
    if text.chars().count() > MAX_BLOCK_CHARS {
        text = format!(
            "<key-health>{count} {keys} for {provider} cooling down ({kind}) until {clock}</key-health>",
            keys = key_word(count),
            clock = clock(until),
        );
    }
    Some(text)
}

fn key_word(count: usize) -> &'static str {
    if count == 1 { "key" } else { "keys" }
}

/// `HH:MM UTC` from epoch milliseconds.
fn clock(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000).rem_euclid(1_440);
    format!("{:02}:{:02} UTC", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oc_llm::FailureKind;
    use oc_store::NewCredential;
    use serde_json::json;

    fn entry(
        provider: &str,
        credential: Option<&str>,
        until_ms: i64,
        kind: FailureKind,
    ) -> KeyHealthEntry {
        KeyHealthEntry {
            provider: provider.to_owned(),
            credential_id: credential.map(str::to_owned),
            until_ms,
            kind,
            failures: 2,
        }
    }

    #[test]
    fn no_cooldown_means_no_prompt_block() {
        let totals = BTreeMap::new();
        assert!(
            block_from(&[], &totals).is_none(),
            "a healthy pool injects no block"
        );
    }

    #[test]
    fn a_cooling_key_names_the_provider_and_says_cooling() {
        let cooling = vec![entry(
            "opencode-go",
            Some("cred_backup"),
            1_794_709_019_047,
            FailureKind::Quota,
        )];
        let mut totals = BTreeMap::new();
        totals.insert("opencode-go".to_owned(), 2);
        let text = block_from(&cooling, &totals).expect("block");
        assert!(text.starts_with("<key-health>"), "{text}");
        assert!(text.contains("opencode-go"), "{text}");
        assert!(text.contains("cooling"), "{text}");
        assert!(text.contains("1 key for"), "{text}");
        assert!(text.contains("1 key remains usable"), "{text}");
        assert!(text.ends_with("</key-health>"), "{text}");
        assert!(
            !text.contains("cred_backup"),
            "the block never names a credential id: {text}"
        );
        assert!(text.chars().count() <= MAX_BLOCK_CHARS, "{text}");
    }

    #[tokio::test]
    async fn stored_key_material_never_reaches_the_tool_data() {
        let store = Store::open_memory().await.expect("store");
        store
            .create_credential(NewCredential {
                id: Some("cred_primary".to_owned()),
                integration_id: "test".to_owned(),
                label: Some("Primary".to_owned()),
                value: json!({"type": "key", "key": "sk-sentinel-9f3c"}),
                active: Some(true),
            })
            .await
            .expect("credential");
        let pool = KeyPool::new();
        pool.mark("test", Some("cred_primary"), FailureKind::Quota, 1_000);
        let env = |_provider: &str| true;

        let providers = collect_key_health(&store, &pool, &env, None, 2_000)
            .await
            .expect("providers");
        assert_eq!(providers.len(), 1);
        let keys = &providers[0].keys;
        assert_eq!(keys.len(), 2, "stored key plus the environment slot");
        assert_eq!(keys[0].credential_id.as_deref(), Some("cred_primary"));
        assert_eq!(keys[0].label.as_deref(), Some("Primary"));
        assert!(keys[0].active);
        assert!(keys[0].limited);
        assert_eq!(keys[0].kind.as_deref(), Some("quota"));
        assert_eq!(keys[0].failures, Some(1));
        assert_eq!(
            keys[1].credential_id, None,
            "the environment key owns no credential row"
        );
        assert_eq!(keys[1].source.as_deref(), Some("env"));
        assert!(!keys[1].limited);

        let rendered = format!("{providers:?}");
        assert!(
            !rendered.contains("sk-sentinel"),
            "key material stays out of the tool data: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_provider_filter_returns_only_that_provider() {
        let store = Store::open_memory().await.expect("store");
        for (id, integration) in [("cred_a", "alpha"), ("cred_b", "beta")] {
            store
                .create_credential(NewCredential {
                    id: Some(id.to_owned()),
                    integration_id: integration.to_owned(),
                    label: None,
                    value: json!({"type": "key", "key": "sk-secret"}),
                    active: Some(true),
                })
                .await
                .expect("credential");
        }
        let pool = KeyPool::new();
        let env = |_provider: &str| false;
        let all = collect_key_health(&store, &pool, &env, None, 1_000)
            .await
            .expect("providers");
        assert_eq!(all.len(), 2, "both integrations are listed");
        let filtered = collect_key_health(&store, &pool, &env, Some("beta"), 1_000)
            .await
            .expect("providers");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider_id, "beta");
        assert_eq!(filtered[0].keys[0].label.as_deref(), Some("default"));
    }
}
