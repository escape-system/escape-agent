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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use oc_llm::{KeyHealthEntry, KeyPool};
use oc_store::{CredentialRow, Store, StoreError};
use oc_tools::key_health::{KeyHealthKey, KeyHealthProvider, KeyHealthSource};

use crate::runtime::Runtime;

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
