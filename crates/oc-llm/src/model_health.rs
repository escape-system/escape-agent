//! Short-lived model availability memory shared by the fallback chain.
//!
//! When a provider fails in a class that says something about the *model*
//! rather than the request, the runtime records it here so the next spawns do
//! not pay the same failed round trip. Entries expire: a retired model is
//! remembered for a day, a quota exhaustion for five minutes, a rate limit for
//! one, a transport blip for thirty seconds. Auth, invalid-request, empty
//! response and unclassified failures are never remembered — they say nothing
//! about whether the model can serve a later request.
//!
//! Keys are variant-insensitive `provider/id`, normalized the same way the
//! catalog resolver and `oc-subagents` compare references, so marking
//! `Anthropic/Claude.Opus#high` hides `anthropic/claude-opus` too.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use crate::error::FailureKind;

/// One recorded unavailability window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    /// Epoch milliseconds at which the entry stops applying (exclusive: an
    /// entry is expired at exactly `until_ms`).
    pub until_ms: i64,
    /// Failure class that produced the entry.
    pub kind: FailureKind,
    /// Provider message, when there was one. Kept for logs and the registry.
    pub message: Option<String>,
}

/// Process-wide model health table. Cheap to share: the runtime owns one and
/// hands out `&Arc<ModelHealth>`.
#[derive(Debug, Default)]
pub struct ModelHealth {
    entries: RwLock<BTreeMap<String, Unavailable>>,
}

impl ModelHealth {
    /// Empty table, shared by the owning runtime.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record `reference` as unavailable for this failure class.
    ///
    /// Kinds that say nothing about model availability ([`FailureKind::Auth`],
    /// [`FailureKind::InvalidRequest`], [`FailureKind::EmptyResponse`],
    /// [`FailureKind::Unknown`]) are a no-op. An existing entry for the same
    /// model is replaced unless it already extends further into the future, so
    /// a later failing spawn cannot shorten a longer window by accident.
    pub fn mark(&self, reference: &str, kind: FailureKind, now_ms: i64, message: Option<String>) {
        let Some(ttl) = ttl_ms(kind) else {
            return;
        };
        let key = normalize_key(reference);
        let until_ms = now_ms.saturating_add(ttl);
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        if entries
            .get(&key)
            .is_some_and(|existing| existing.until_ms >= until_ms)
        {
            return;
        }
        entries.insert(
            key,
            Unavailable {
                until_ms,
                kind,
                message,
            },
        );
    }

    /// False when `reference` has an unexpired entry. Expired entries are
    /// dropped on the way out.
    #[must_use]
    pub fn is_available(&self, reference: &str, now_ms: i64) -> bool {
        let key = normalize_key(reference);
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        match entries.get(&key) {
            Some(entry) if entry.until_ms > now_ms => false,
            Some(_) => {
                entries.remove(&key);
                true
            }
            None => true,
        }
    }

    /// Snapshot of the unexpired entries, pruned in place and sorted by
    /// normalized reference (the `BTreeMap` order).
    pub fn entries(&self, now_ms: i64) -> Vec<(String, Unavailable)> {
        let mut entries = self.entries.write().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, entry| entry.until_ms > now_ms);
        entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect()
    }

    /// Forget every entry (config reloads and explicit repairs use this).
    pub fn clear(&self) {
        self.entries
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// How long a failure class keeps a model out of rotation.
///
/// `None` means "do not remember": the failure is about the request or the
/// credential, not the model's continued existence.
fn ttl_ms(kind: FailureKind) -> Option<i64> {
    match kind {
        FailureKind::ModelUnavailable => Some(86_400_000),
        FailureKind::Quota => Some(300_000),
        FailureKind::RateLimit => Some(60_000),
        FailureKind::Transport => Some(30_000),
        FailureKind::Auth
        | FailureKind::InvalidRequest
        | FailureKind::EmptyResponse
        | FailureKind::Unknown => None,
    }
}

/// Variant-insensitive `provider/id`: everything after `#` is dropped and the
/// remainder is lowercased with `.` folded to `-`, matching the catalog
/// resolver's alias handling.
fn normalize_key(reference: &str) -> String {
    let base = reference
        .split_once('#')
        .map_or(reference, |(base, _variant)| base);
    base.to_ascii_lowercase().replace('.', "-")
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.message {
            Some(message) => write!(f, "{} until {}: {message}", self.kind, self.until_ms),
            None => write!(f, "{} until {}", self.kind, self.until_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 3_600_000;

    #[test]
    fn mark_then_read() {
        let health = ModelHealth::new();
        assert!(health.is_available("anthropic/claude-opus-4-6", 0));
        health.mark(
            "anthropic/claude-opus-4-6",
            FailureKind::Quota,
            1_000,
            Some("out of credit".to_owned()),
        );
        assert!(!health.is_available("anthropic/claude-opus-4-6", 1_000));
        assert!(!health.is_available("anthropic/claude-opus-4-6", 301_000 - 1));
        let entries = health.entries(1_000);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "anthropic/claude-opus-4-6");
        assert_eq!(entries[0].1.until_ms, 1_000 + 300_000);
        assert_eq!(entries[0].1.kind, FailureKind::Quota);
        assert_eq!(entries[0].1.message.as_deref(), Some("out of credit"));
    }

    #[test]
    fn entries_expire_at_their_until() {
        let health = ModelHealth::new();
        health.mark("openai/gpt-5", FailureKind::Transport, 0, None);
        assert!(!health.is_available("openai/gpt-5", 29_999));
        assert!(
            health.is_available("openai/gpt-5", 30_000),
            "an entry is expired at exactly until_ms"
        );
        assert!(
            health.entries(30_000).is_empty(),
            "reads prune expired rows"
        );
    }

    #[test]
    fn keys_ignore_case_dots_and_variants() {
        let health = ModelHealth::new();
        health.mark(
            "Anthropic/Claude.Opus#high",
            FailureKind::RateLimit,
            0,
            None,
        );
        assert!(!health.is_available("anthropic/claude-opus", 0));
        assert!(!health.is_available("ANTHROPIC/CLAUDE-OPUS#low", 0));
        assert_eq!(health.entries(0)[0].0, "anthropic/claude-opus");
    }

    #[test]
    fn longer_windows_survive_shorter_marks() {
        let health = ModelHealth::new();
        health.mark(
            "google/gemini-3-pro",
            FailureKind::ModelUnavailable,
            0,
            None,
        );
        health.mark("google/gemini-3-pro", FailureKind::RateLimit, 1_000, None);
        let entries = health.entries(1_000);
        assert_eq!(entries[0].1.kind, FailureKind::ModelUnavailable);
        assert_eq!(entries[0].1.until_ms, 86_400_000);
        // An expired entry is always replaced.
        health.mark(
            "google/gemini-3-pro",
            FailureKind::RateLimit,
            90_000_000,
            None,
        );
        let entries = health.entries(90_000_000);
        assert_eq!(entries[0].1.kind, FailureKind::RateLimit);
        assert_eq!(entries[0].1.until_ms, 90_060_000);
    }

    #[test]
    fn clear_forgets_everything() {
        let health = ModelHealth::new();
        health.mark("openai/gpt-5", FailureKind::ModelUnavailable, 0, None);
        assert!(!health.entries(0).is_empty());
        health.clear();
        assert!(health.entries(0).is_empty());
        assert!(health.is_available("openai/gpt-5", 0));
    }

    #[test]
    fn request_scoped_kinds_are_never_marked() {
        let health = ModelHealth::new();
        for kind in [
            FailureKind::Auth,
            FailureKind::InvalidRequest,
            FailureKind::EmptyResponse,
            FailureKind::Unknown,
        ] {
            health.mark("openai/gpt-5", kind, 0, Some("nope".to_owned()));
        }
        assert!(
            health.entries(0).is_empty(),
            "only model-level classes are remembered"
        );
        assert!(health.is_available("openai/gpt-5", 0));
    }

    #[test]
    fn entries_are_sorted_and_pruned_together() {
        let health = ModelHealth::new();
        health.mark("zeta/model", FailureKind::Transport, 0, None);
        health.mark("alpha/model", FailureKind::Quota, HOUR_MS, None);
        health.mark("beta/model", FailureKind::RateLimit, HOUR_MS, None);
        let keys: Vec<String> = health
            .entries(HOUR_MS)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, vec!["alpha/model", "beta/model"]);
    }
}
