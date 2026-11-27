//! Short-lived per-credential key rotation memory.
//!
//! This is deliberately *not* [`ModelHealth`](crate::model_health::ModelHealth).
//! `ModelHealth` is scoped to `provider/id`: when it retires a model, every
//! caller of that model stops trying it. A credential is a different axis — one
//! provider usually has several keys, and an exhausted first key says nothing
//! about whether the second one can serve the *same* model. Hiding a model
//! because key A ran out of quota would suppress a model that key B still
//! serves. `Auth` is the sharpest case: a revoked key must rotate away from
//! that key only, never advance the model chain.
//!
//! So this table is keyed `provider -> credential id` (or [`ENV_KEY_ID`] for
//! keys that come from the environment rather than the credential store) and
//! tracks one cooldown per key:
//!
//! - `Quota` — five minutes; the key cannot be expected to serve soon.
//! - `RateLimit` — one minute; the window is usually short.
//! - `Auth` — thirty minutes; a rotated/revoked key stays out of rotation.
//! - `Transport` — thirty seconds; a network blip, not a credential problem.
//!
//! `ModelUnavailable`, `InvalidRequest`, `EmptyResponse` and `Unknown` are
//! never recorded here: they are about the model or the request, and retrying
//! with a different key would repeat the same failure.
//!
//! Nothing is persisted and nothing is logged. After a restart the pool falls
//! back to the caller's priority order (active key first), exactly like
//! `ModelHealth::clear`. Key material is never formatted: [`PoolKey`] has a
//! hand-written `Debug` that redacts it.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use zeroize::Zeroizing;

use crate::error::FailureKind;

/// Credential id used for keys that do not come from a stored credential.
pub const ENV_KEY_ID: &str = "__env__";

/// One candidate key for a provider request.
///
/// The caller builds these in priority order (active credential first). The
/// pool only filters them; it never reorders, and never dedupes two entries
/// with identical key material, because distinct credential ids keep distinct
/// labels and distinct rotation state. Key bytes are zeroized on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct PoolKey {
    /// Stored credential this key came from, or `None` for an environment key.
    pub credential_id: Option<String>,
    /// Secret material wrapped in [`Zeroizing`]. Never formatted, never serialized.
    pub key: Zeroizing<String>,
}

impl PoolKey {
    /// Construct a [`PoolKey`] wrapping secret bytes in [`Zeroizing`].
    #[must_use]
    pub fn new(credential_id: Option<String>, key: impl Into<String>) -> Self {
        Self {
            credential_id,
            key: Zeroizing::new(key.into()),
        }
    }

    /// Borrow the underlying secret key string.
    #[must_use]
    pub fn secret(&self) -> &str {
        self.key.as_str()
    }
}

/// Sanitized startup credential-presence report (`detect_auth()`).
///
/// Checks whether environment variables or stored credentials exist without
/// logging or copying secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthDetection {
    pub provider: String,
    pub env_vars_present: Vec<String>,
    pub has_stored_credential: bool,
    pub available: bool,
}

/// Detect which providers have credentials configured via env vars or store ids
/// without exposing secret values.
#[must_use]
pub fn detect_auth(specs: &[(&str, &[&str])], stored_providers: &[&str]) -> Vec<AuthDetection> {
    specs
        .iter()
        .map(|(provider, envs)| {
            let env_vars_present: Vec<String> = envs
                .iter()
                .filter(|name| std::env::var_os(name).is_some_and(|val| !val.is_empty()))
                .map(|name| (*name).to_owned())
                .collect();
            let has_stored_credential = stored_providers
                .iter()
                .any(|stored| stored.eq_ignore_ascii_case(provider));
            let available = !env_vars_present.is_empty() || has_stored_credential;
            AuthDetection {
                provider: (*provider).to_owned(),
                env_vars_present,
                has_stored_credential,
                available,
            }
        })
        .collect()
}

impl fmt::Debug for PoolKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolKey")
            .field("credential_id", &self.credential_id)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// One recorded cooldown window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyHealthEntry {
    /// Provider the key belongs to (normalized, lowercase).
    pub provider: String,
    /// Credential id, or `None` for the provider's environment key.
    pub credential_id: Option<String>,
    /// Epoch milliseconds at which the cooldown stops applying (exclusive: an
    /// entry is expired at exactly `until_ms`).
    pub until_ms: i64,
    /// Failure class that produced the entry.
    pub kind: FailureKind,
    /// Failures recorded for this key while the entry is retained. The counter
    /// starts over when the entry is pruned, so a key that recovers and later
    /// fails again is not blamed for the old outage.
    pub failures: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyState {
    until_ms: i64,
    kind: FailureKind,
    failures: u32,
}

/// Process-wide key rotation table. The runtime owns one and hands out
/// `&Arc<KeyPool>`; every provider call asks it which keys are still usable.
#[derive(Debug, Default)]
pub struct KeyPool {
    /// `provider -> credential id (or `__env__`) -> cooldown`.
    entries: RwLock<BTreeMap<String, BTreeMap<String, KeyState>>>,
    /// Process-local per-provider round-robin cursor for balanced healthy key selection.
    cursors: RwLock<BTreeMap<String, usize>>,
}

impl KeyPool {
    /// Empty table, shared by the owning runtime.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The usable subset of `keys` at `now_ms`, in the caller's order.
    ///
    /// Keys whose cooldown is still in the future are dropped; healthy keys are
    /// returned exactly as supplied, so the caller's active-first priority
    /// decides which key a request uses. Expired rows for this provider are
    /// pruned on the way out.
    #[must_use]
    pub fn select(&self, provider: &str, keys: Vec<PoolKey>, now_ms: i64) -> Vec<PoolKey> {
        let provider = normalize_provider(provider);
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let selected = {
            let Some(state) = entries.get_mut(&provider) else {
                return keys;
            };
            state.retain(|_, entry| entry.until_ms > now_ms);
            keys.into_iter()
                .filter(|key| !state.contains_key(&pool_id(key.credential_id.as_deref())))
                .collect()
        };
        if entries.get(&provider).is_some_and(BTreeMap::is_empty) {
            entries.remove(&provider);
        }
        selected
    }

    /// Start (or extend) a cooldown for one key.
    ///
    /// Kinds that say nothing about the key ([`FailureKind::ModelUnavailable`],
    /// [`FailureKind::InvalidRequest`], [`FailureKind::EmptyResponse`],
    /// [`FailureKind::Unknown`]) are a no-op. The failure counter always moves,
    /// but an existing cooldown keeps its longer `until_ms`, so a later,
    /// shorter failure cannot bring a key back early.
    pub fn mark(
        &self,
        provider: &str,
        credential_id: Option<&str>,
        kind: FailureKind,
        now_ms: i64,
    ) {
        let Some(ttl) = ttl_ms(kind) else {
            return;
        };
        let provider = normalize_provider(provider);
        let id = pool_id(credential_id);
        let until_ms = now_ms.saturating_add(ttl);
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let state = entries.entry(provider).or_default();
        match state.get_mut(&id) {
            Some(existing) if existing.until_ms >= until_ms => {
                existing.failures = existing.failures.saturating_add(1);
            }
            Some(existing) => {
                existing.until_ms = until_ms;
                existing.kind = kind;
                existing.failures = existing.failures.saturating_add(1);
            }
            None => {
                state.insert(
                    id,
                    KeyState {
                        until_ms,
                        kind,
                        failures: 1,
                    },
                );
            }
        }
    }

    /// Snapshot of the unexpired cooldowns, pruned in place and sorted by
    /// provider and then credential id.
    #[must_use]
    pub fn entries(&self, now_ms: i64) -> Vec<KeyHealthEntry> {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let mut snapshot = Vec::new();
        entries.retain(|provider, keys| {
            keys.retain(|_, key| key.until_ms > now_ms);
            for (id, key) in keys.iter() {
                snapshot.push(KeyHealthEntry {
                    provider: provider.clone(),
                    credential_id: credential_id_of(id),
                    until_ms: key.until_ms,
                    kind: key.kind,
                    failures: key.failures,
                });
            }
            !keys.is_empty()
        });
        snapshot
    }

    /// Forget every cooldown (config reloads and explicit repairs use this).
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.cursors
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Return whether `credential_id` (or `ENV_KEY_ID` when `None`) is cooling at `now_ms`.
    #[must_use]
    pub fn is_cooling(&self, provider: &str, credential_id: Option<&str>, now_ms: i64) -> bool {
        let provider = normalize_provider(provider);
        let id = pool_id(credential_id);
        let entries = self.entries.read().unwrap_or_else(PoisonError::into_inner);
        entries
            .get(&provider)
            .and_then(|state| state.get(&id))
            .is_some_and(|entry| entry.until_ms > now_ms)
    }

    /// Pick the next unexpired healthy credential id from `candidate_ids` in round-robin
    /// order so parallel subagent spawns spread load across healthy keys on one provider.
    /// Returns `None` if `candidate_ids` has no healthy non-cooling entry.
    #[must_use]
    pub fn pick_balanced_credential(
        &self,
        provider: &str,
        candidate_ids: &[Option<String>],
        now_ms: i64,
    ) -> Option<Option<String>> {
        let provider = normalize_provider(provider);
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        let usable: Vec<Option<String>> = if let Some(state) = entries.get_mut(&provider) {
            state.retain(|_, entry| entry.until_ms > now_ms);
            candidate_ids
                .iter()
                .filter(|id| !state.contains_key(&pool_id(id.as_deref())))
                .cloned()
                .collect()
        } else {
            candidate_ids.to_vec()
        };
        if entries.get(&provider).is_some_and(BTreeMap::is_empty) {
            entries.remove(&provider);
        }
        drop(entries);

        if usable.is_empty() {
            return None;
        }
        let mut cursors = self.cursors.write().unwrap_or_else(PoisonError::into_inner);
        let cursor = cursors.entry(provider).or_insert(0);
        let index = *cursor % usable.len();
        *cursor = cursor.wrapping_add(1);
        Some(usable[index].clone())
    }
}

/// How long a failure class keeps one key out of rotation.
///
/// `None` means "do not remember": the failure is about the model or the
/// request, and the next key would fail the same way.
fn ttl_ms(kind: FailureKind) -> Option<i64> {
    match kind {
        FailureKind::Quota => Some(300_000),
        FailureKind::RateLimit => Some(60_000),
        FailureKind::Auth => Some(1_800_000),
        FailureKind::Transport => Some(30_000),
        FailureKind::ModelUnavailable
        | FailureKind::InvalidRequest
        | FailureKind::PolicyBlocked
        | FailureKind::EmptyResponse
        | FailureKind::Unknown => None,
    }
}

/// Table key for a credential: the id itself, or [`ENV_KEY_ID`] when absent.
fn pool_id(credential_id: Option<&str>) -> String {
    credential_id.unwrap_or(ENV_KEY_ID).to_owned()
}

/// Inverse of [`pool_id`] for snapshots: the environment key has no id.
fn credential_id_of(id: &str) -> Option<String> {
    if id == ENV_KEY_ID {
        None
    } else {
        Some(id.to_owned())
    }
}

/// Provider ids compare case-insensitively and ignore surrounding space, the
/// same way the catalog resolver matches provider names.
fn normalize_provider(provider: &str) -> String {
    provider.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(credential_id: Option<&str>, secret: &str) -> PoolKey {
        PoolKey::new(credential_id.map(str::to_owned), secret)
    }

    #[test]
    fn select_drops_cooled_keys_and_keeps_caller_order() {
        let pool = KeyPool::new();
        pool.mark("anthropic", Some("cred_a"), FailureKind::Quota, 1_000);
        let keys = vec![
            key(Some("cred_b"), "secret-b"),
            key(Some("cred_a"), "secret-a"),
            key(None, "secret-env"),
        ];
        let selected = pool.select("anthropic", keys.clone(), 1_000);
        assert_eq!(
            selected,
            vec![key(Some("cred_b"), "secret-b"), key(None, "secret-env"),]
        );
        // The incoming order is preserved exactly, even when the caller lists
        // a cooled key first.
        let selected = pool.select("anthropic", keys, 1_000 + 300_000);
        assert_eq!(
            selected,
            vec![
                key(Some("cred_b"), "secret-b"),
                key(Some("cred_a"), "secret-a"),
                key(None, "secret-env"),
            ],
            "an entry is usable again at exactly until_ms"
        );
    }

    #[test]
    fn select_never_dedupes_identical_key_material() {
        let pool = KeyPool::new();
        let keys = vec![
            key(Some("cred_a"), "same-secret"),
            key(Some("cred_b"), "same-secret"),
        ];
        assert_eq!(pool.select("openai", keys.clone(), 0), keys);
    }

    #[test]
    fn env_keys_are_tracked_separately_from_stored_keys() {
        let pool = KeyPool::new();
        pool.mark("openai", None, FailureKind::RateLimit, 0);
        let keys = vec![key(Some("cred_a"), "secret-a"), key(None, "secret-env")];
        assert_eq!(
            pool.select("openai", keys, 0),
            vec![key(Some("cred_a"), "secret-a")]
        );
        let entries = pool.entries(0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].provider, "openai");
        assert_eq!(entries[0].credential_id, None);
    }

    #[test]
    fn each_class_uses_its_own_cooldown() {
        for (kind, ttl) in [
            (FailureKind::Quota, 300_000),
            (FailureKind::RateLimit, 60_000),
            (FailureKind::Auth, 1_800_000),
            (FailureKind::Transport, 30_000),
        ] {
            let pool = KeyPool::new();
            pool.mark("google", Some("cred_a"), kind, 1_000);
            let entries = pool.entries(1_000);
            assert_eq!(entries.len(), 1, "{kind} must be recorded");
            assert_eq!(entries[0].until_ms, 1_000 + ttl, "{kind} cooldown");
            assert_eq!(entries[0].kind, kind);
            assert_eq!(entries[0].failures, 1);
        }
    }

    #[test]
    fn model_and_request_scoped_kinds_are_never_marked() {
        let pool = KeyPool::new();
        for kind in [
            FailureKind::ModelUnavailable,
            FailureKind::InvalidRequest,
            FailureKind::EmptyResponse,
            FailureKind::Unknown,
        ] {
            pool.mark("google", Some("cred_a"), kind, 0);
        }
        assert!(
            pool.entries(0).is_empty(),
            "only key-level classes are remembered"
        );
        let keys = vec![key(Some("cred_a"), "secret-a")];
        assert_eq!(
            pool.select("google", keys.clone(), 0),
            keys,
            "a model-level failure leaves the key usable"
        );
    }

    #[test]
    fn shorter_marks_never_shorten_a_longer_cooldown() {
        let pool = KeyPool::new();
        pool.mark("google", Some("cred_a"), FailureKind::Auth, 0);
        pool.mark("google", Some("cred_a"), FailureKind::RateLimit, 1_000);
        let entries = pool.entries(1_000);
        assert_eq!(entries[0].until_ms, 1_800_000);
        assert_eq!(entries[0].kind, FailureKind::Auth);
        assert_eq!(entries[0].failures, 2);
        // An expired entry is always replaced by a fresh window.
        pool.mark("google", Some("cred_a"), FailureKind::RateLimit, 1_800_000);
        let entries = pool.entries(1_800_000);
        assert_eq!(entries[0].until_ms, 1_800_000 + 60_000);
        assert_eq!(entries[0].kind, FailureKind::RateLimit);
        assert_eq!(entries[0].failures, 3);
    }

    #[test]
    fn entries_prune_expired_and_sort() {
        let pool = KeyPool::new();
        pool.mark("zeta", Some("cred_z"), FailureKind::Transport, 0);
        pool.mark("alpha", Some("cred_b"), FailureKind::Quota, 100_000);
        pool.mark("alpha", Some("cred_a"), FailureKind::RateLimit, 100_000);
        let entries = pool.entries(100_000);
        let listed: Vec<(&str, Option<&str>)> = entries
            .iter()
            .map(|entry| (entry.provider.as_str(), entry.credential_id.as_deref()))
            .collect();
        assert_eq!(
            listed,
            vec![("alpha", Some("cred_a")), ("alpha", Some("cred_b"))],
            "expired rows are dropped and the rest sort by provider then id"
        );
    }

    #[test]
    fn clear_forgets_everything() {
        let pool = KeyPool::new();
        pool.mark("openai", Some("cred_a"), FailureKind::Quota, 0);
        assert!(!pool.entries(0).is_empty());
        pool.clear();
        assert!(pool.entries(0).is_empty());
        let keys = vec![key(Some("cred_a"), "secret-a")];
        assert_eq!(pool.select("openai", keys.clone(), 0), keys);
    }

    #[test]
    fn provider_ids_compare_case_insensitively() {
        let pool = KeyPool::new();
        pool.mark(" OpenAI ", Some("cred_a"), FailureKind::Quota, 0);
        let keys = vec![key(Some("cred_a"), "secret-a")];
        assert!(pool.select("openai", keys, 0).is_empty());
    }

    #[test]
    fn debug_redacts_key_material() {
        let rendered = format!("{:?}", key(Some("cred_a"), "sk-super-secret"));
        assert!(!rendered.contains("sk-super-secret"), "got {rendered}");
        assert!(rendered.contains("<redacted>"), "got {rendered}");
        assert!(rendered.contains("cred_a"), "got {rendered}");
    }

    #[test]
    fn pick_balanced_credential_rotates_and_skips_cooling_keys() {
        let pool = KeyPool::new();
        let candidates = vec![Some("cred_a".to_owned()), Some("cred_b".to_owned()), None];
        pool.mark("opencode-go", Some("cred_b"), FailureKind::RateLimit, 1_000);
        let first = pool.pick_balanced_credential("opencode-go", &candidates, 1_000);
        let second = pool.pick_balanced_credential("opencode-go", &candidates, 1_000);
        let third = pool.pick_balanced_credential("opencode-go", &candidates, 1_000);
        assert_eq!(first, Some(Some("cred_a".to_owned())));
        assert_eq!(second, Some(None));
        assert_eq!(third, Some(Some("cred_a".to_owned())));
        assert!(pool.is_cooling("opencode-go", Some("cred_b"), 1_000));
        assert!(!pool.is_cooling("opencode-go", Some("cred_b"), 70_000));
    }
}
