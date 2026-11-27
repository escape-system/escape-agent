//! Dual-TTL (memory + disk) thought-signature cache for Antigravity multi-turn conversations.

use crate::accounts::ModelFamily;
use crate::config::{SignatureCacheConfig, resolve_config_dir};
use crate::error::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cached signed thinking entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedSignatureEntry {
    /// Opaque upstream `thoughtSignature` token.
    pub signature: String,
    /// Origin model family (`"claude"` or `"gemini"`).
    pub family: ModelFamily,
    /// Creation time (epoch ms).
    pub created_at: i64,
    /// Last access time (epoch ms).
    pub last_accessed: i64,
    /// Optional session key that stored the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
}

/// Hard cap on in-memory & persisted signature entries to prevent unbounded RAM growth.
const MAX_CACHE_ENTRIES: usize = 2_000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedSignatureStore {
    version: u32,
    entries: HashMap<String, CachedSignatureEntry>,
}

#[derive(Serialize)]
struct BorrowedSignatureStore<'a> {
    version: u32,
    entries: &'a HashMap<String, CachedSignatureEntry>,
}

/// Dual-TTL signature cache keyed by `(session_key, text_hash)` with global text-hash fallback.
#[derive(Debug)]
pub struct SignatureCache {
    cfg: SignatureCacheConfig,
    path: Option<PathBuf>,
    entries: HashMap<String, CachedSignatureEntry>,
    dirty: bool,
    last_write_ms: i64,
}

impl SignatureCache {
    /// Load cache from `path` (if configured) and apply [`SignatureCacheConfig`].
    pub fn load(path: Option<PathBuf>, cfg: SignatureCacheConfig, now_ms: i64) -> Result<Self> {
        let mut entries = HashMap::new();
        if let Some(ref p) = path
            && p.is_file()
            && let Ok(raw) = std::fs::read_to_string(p)
            && !raw.trim().is_empty()
            && let Ok(store) = serde_json::from_str::<PersistedSignatureStore>(&raw)
        {
            let disk_ttl_ms =
                i64::try_from(cfg.disk_ttl_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
            for (k, v) in store.entries {
                if now_ms.saturating_sub(v.last_accessed) <= disk_ttl_ms {
                    entries.insert(k, v);
                }
            }
        }
        let mut cache = Self {
            cfg,
            path,
            entries,
            dirty: false,
            last_write_ms: now_ms,
        };
        cache.evict_excess();
        Ok(cache)
    }

    /// Default path `<config_dir>/antigravity-signature-cache.json`.
    #[must_use]
    pub fn default_path(config_dir: Option<&Path>) -> PathBuf {
        config_dir
            .map_or_else(resolve_config_dir, PathBuf::from)
            .join("antigravity-signature-cache.json")
    }

    /// Compute a stable key from session identifier and thinking text.
    #[must_use]
    pub fn composite_key(session_key: &str, thinking_text: &str) -> String {
        format!("{session_key}:{}", hash_thinking_text(thinking_text))
    }

    /// Store a signature associated with `(session_key, thinking_text)` and also index by global text hash.
    pub fn store(
        &mut self,
        session_key: &str,
        thinking_text: &str,
        signature: &str,
        family: ModelFamily,
        now_ms: i64,
    ) {
        if !self.cfg.enabled || signature.trim().is_empty() || thinking_text.trim().is_empty() {
            return;
        }
        let text_hash = hash_thinking_text(thinking_text);
        let entry = CachedSignatureEntry {
            signature: signature.to_owned(),
            family,
            created_at: now_ms,
            last_accessed: now_ms,
            session_key: Some(session_key.to_owned()),
        };
        if !session_key.trim().is_empty() {
            self.entries
                .insert(format!("{session_key}:{text_hash}"), entry.clone());
        }
        self.entries.insert(format!("global:{text_hash}"), entry);
        self.evict_excess();
        self.dirty = true;
    }

    fn evict_excess(&mut self) {
        if self.entries.len() <= MAX_CACHE_ENTRIES {
            return;
        }
        let excess = self.entries.len() - MAX_CACHE_ENTRIES;
        let mut by_age: Vec<(i64, String)> = self
            .entries
            .iter()
            .map(|(k, v)| (v.last_accessed, k.clone()))
            .collect();
        if excess < by_age.len() {
            by_age.select_nth_unstable_by(excess, |a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            for (_, key) in &by_age[..excess] {
                self.entries.remove(key);
            }
        }
    }

    /// Look up a valid cached signature by exact `(session_key, thinking_text)` or global text hash.
    pub fn get(
        &mut self,
        session_key: &str,
        thinking_text: &str,
        now_ms: i64,
    ) -> Option<CachedSignatureEntry> {
        if !self.cfg.enabled || thinking_text.trim().is_empty() {
            return None;
        }
        let mem_ttl_ms =
            i64::try_from(self.cfg.memory_ttl_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
        let text_hash = hash_thinking_text(thinking_text);

        let keys = if session_key.trim().is_empty() {
            vec![format!("global:{text_hash}")]
        } else {
            vec![
                format!("{session_key}:{text_hash}"),
                format!("global:{text_hash}"),
            ]
        };

        for key in keys {
            if let Some(entry) = self.entries.get_mut(&key)
                && now_ms.saturating_sub(entry.last_accessed) <= mem_ttl_ms
            {
                entry.last_accessed = now_ms;
                self.dirty = true;
                return Some(entry.clone());
            }
        }
        None
    }

    /// Prepare a dirty cache snapshot for off-thread persistence if due.
    pub fn prepare_flush(&mut self, now_ms: i64, force: bool) -> Result<Option<(PathBuf, String)>> {
        if !self.cfg.enabled || !self.dirty {
            return Ok(None);
        }
        let interval_ms =
            i64::try_from(self.cfg.write_interval_seconds.saturating_mul(1000)).unwrap_or(60_000);
        if !force && now_ms.saturating_sub(self.last_write_ms) < interval_ms {
            return Ok(None);
        }

        let disk_ttl_ms =
            i64::try_from(self.cfg.disk_ttl_seconds.saturating_mul(1000)).unwrap_or(i64::MAX);
        self.entries
            .retain(|_, v| now_ms.saturating_sub(v.last_accessed) <= disk_ttl_ms);

        let pending = if let Some(ref path) = self.path {
            let payload = BorrowedSignatureStore {
                version: 1,
                entries: &self.entries,
            };
            Some((path.clone(), serde_json::to_string(&payload)?))
        } else {
            None
        };

        self.dirty = false;
        self.last_write_ms = now_ms;
        Ok(pending)
    }

    /// Persist a previously prepared cache snapshot to disk atomically.
    pub fn write_flush_payload(path: &Path, json: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Evict expired entries and persist to disk if dirty and past the configured write interval.
    pub fn flush_if_due(&mut self, now_ms: i64, force: bool) -> Result<()> {
        if let Some((path, json)) = self.prepare_flush(now_ms, force)? {
            Self::write_flush_payload(&path, &json)?;
        }
        Ok(())
    }
}

/// Deterministic short SHA-256 hash of trimmed thinking content.
#[must_use]
pub fn hash_thinking_text(thinking_text: &str) -> String {
    let digest = Sha256::digest(thinking_text.trim().as_bytes());
    hex::encode(&digest[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn signature_cache_stores_retrieves_and_persists_across_reloads() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("sigs.json");
        let cfg = SignatureCacheConfig::default();

        let mut cache = SignatureCache::load(Some(path.clone()), cfg.clone(), 1000).expect("load");
        cache.store(
            "ses_1",
            "Let me inspect main.rs carefully.",
            "sig_abc_123",
            ModelFamily::Gemini,
            1000,
        );
        cache.flush_if_due(1000, true).expect("flush");

        let mut reloaded = SignatureCache::load(Some(path), cfg, 2000).expect("reload");
        let found = reloaded
            .get("ses_1", "Let me inspect main.rs carefully.", 2000)
            .expect("hit");
        assert_eq!(found.signature, "sig_abc_123");

        let fallback = reloaded
            .get("ses_other", "Let me inspect main.rs carefully.", 2000)
            .expect("global fallback");
        assert_eq!(fallback.signature, "sig_abc_123");
    }

    #[test]
    fn signature_cache_bounds_entries_at_capacity() {
        let cfg = SignatureCacheConfig::default();
        let mut cache = SignatureCache::load(None, cfg, 1_000).expect("load");
        for i in 0..=MAX_CACHE_ENTRIES {
            let now = 1_000 + i64::try_from(i).expect("idx");
            cache.store(
                &format!("ses_{i}"),
                &format!("unique thinking prompt #{i}"),
                "sig_val",
                ModelFamily::Gemini,
                now,
            );
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        let check_now = 1_000 + i64::try_from(MAX_CACHE_ENTRIES).expect("idx");
        assert!(
            cache
                .get("ses_0", "unique thinking prompt #0", check_now)
                .is_none(),
            "oldest entry must be evicted when capacity is exceeded"
        );
        let newest = MAX_CACHE_ENTRIES;
        assert!(
            cache
                .get(
                    &format!("ses_{newest}"),
                    &format!("unique thinking prompt #{newest}"),
                    check_now,
                )
                .is_some(),
            "most recent entry must remain cached"
        );
    }
}
