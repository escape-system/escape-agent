//! Bounded per-client diagnostics cache.
//!
//! Servers push diagnostics and, when they advertise it, answer pull requests.
//! Both sources are stored per path and merged on read. The cache is bounded by
//! path count *and* estimated JSON bytes, with LRU eviction, because language
//! servers happily publish tens of megabytes for large workspaces.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Default maximum number of cached paths per client.
pub const DEFAULT_MAX_PATHS: usize = 10_000;
/// Default maximum estimated bytes per client (8 MiB).
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// A snapshot of the cached diagnostics for one path.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedDiagnostics {
    /// Path the diagnostics belong to.
    pub path: PathBuf,
    /// Version advertised by the server, when it supports versioned publishes.
    pub version: Option<i64>,
    /// Diagnostics delivered by `textDocument/publishDiagnostics`.
    pub push: Vec<Value>,
    /// Diagnostics delivered by `textDocument/diagnostic`.
    pub pull: Vec<Value>,
    /// Result id of the last successful pull, used as `previousResultId`.
    pub result_id: Option<String>,
    /// Epoch milliseconds of the last update.
    pub at: i64,
}

impl CachedDiagnostics {
    /// Push and pull diagnostics merged without duplicates.
    #[must_use]
    pub fn merged(&self) -> Vec<Value> {
        let mut merged = self.push.clone();
        for item in &self.pull {
            if !merged
                .iter()
                .any(|existing| same_diagnostic(existing, item))
            {
                merged.push(item.clone());
            }
        }
        merged
    }

    /// Only severity-1 (`Error`) diagnostics.
    #[must_use]
    pub fn errors(&self) -> Vec<Value> {
        self.merged()
            .into_iter()
            .filter(|item| item.get("severity").and_then(Value::as_i64) == Some(1))
            .collect()
    }
}

fn same_diagnostic(left: &Value, right: &Value) -> bool {
    left.get("range") == right.get("range")
        && left.get("message") == right.get("message")
        && left.get("source") == right.get("source")
        && left.get("code") == right.get("code")
}

#[derive(Debug)]
struct Entry {
    version: Option<i64>,
    push: Vec<Value>,
    pull: Vec<Value>,
    result_id: Option<String>,
    at: i64,
    tick: u64,
    bytes: usize,
}

/// LRU-bounded diagnostics cache.
#[derive(Debug)]
pub struct DiagnosticsCache {
    max_paths: usize,
    max_bytes: usize,
    entries: HashMap<PathBuf, Entry>,
    order: BTreeMap<u64, PathBuf>,
    bytes: usize,
    tick: u64,
}

impl Default for DiagnosticsCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PATHS, DEFAULT_MAX_BYTES)
    }
}

impl DiagnosticsCache {
    /// Create a cache with explicit bounds.
    #[must_use]
    pub fn new(max_paths: usize, max_bytes: usize) -> Self {
        Self {
            max_paths: max_paths.max(1),
            max_bytes: max_bytes.max(1),
            entries: HashMap::new(),
            order: BTreeMap::new(),
            bytes: 0,
            tick: 0,
        }
    }

    /// Number of cached paths.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Estimated bytes currently held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Read a snapshot, refreshing the LRU position.
    pub fn get(&mut self, path: &Path) -> Option<CachedDiagnostics> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(path)?;
        self.order.remove(&entry.tick);
        entry.tick = tick;
        self.order.insert(tick, path.to_path_buf());
        Some(CachedDiagnostics {
            path: path.to_path_buf(),
            version: entry.version,
            push: entry.push.clone(),
            pull: entry.pull.clone(),
            result_id: entry.result_id.clone(),
            at: entry.at,
        })
    }

    /// Read a snapshot without touching the LRU order.
    #[must_use]
    pub fn peek(&self, path: &Path) -> Option<CachedDiagnostics> {
        let entry = self.entries.get(path)?;
        Some(CachedDiagnostics {
            path: path.to_path_buf(),
            version: entry.version,
            push: entry.push.clone(),
            pull: entry.pull.clone(),
            result_id: entry.result_id.clone(),
            at: entry.at,
        })
    }

    /// Replace the push diagnostics. Returns `false` for a stale version.
    pub fn put_push(&mut self, path: &Path, version: Option<i64>, items: Vec<Value>) -> bool {
        if let (Some(new), Some(old)) = (version, self.entries.get(path).and_then(|e| e.version))
            && new < old
        {
            return false;
        }
        let entry = self.entry_mut(path);
        entry.version = version.or(entry.version);
        entry.push = items;
        entry.at = now_millis();
        self.reweigh(path);
        true
    }

    /// Replace the pull diagnostics.
    pub fn put_pull(&mut self, path: &Path, items: Vec<Value>, result_id: Option<String>) {
        let entry = self.entry_mut(path);
        entry.pull = items;
        if result_id.is_some() {
            entry.result_id = result_id;
        }
        entry.at = now_millis();
        self.reweigh(path);
    }

    /// Remove a path (for example after `didClose`).
    pub fn remove(&mut self, path: &Path) {
        if let Some(entry) = self.entries.remove(path) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
            self.order.remove(&entry.tick);
        }
    }

    /// Drop everything, used on client eviction.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn entry_mut(&mut self, path: &Path) -> &mut Entry {
        let tick = self.next_tick();
        if !self.entries.contains_key(path) {
            self.entries.insert(
                path.to_path_buf(),
                Entry {
                    version: None,
                    push: Vec::new(),
                    pull: Vec::new(),
                    result_id: None,
                    at: 0,
                    tick,
                    bytes: 0,
                },
            );
            self.order.insert(tick, path.to_path_buf());
        }
        self.entries.get_mut(path).expect("entry inserted above")
    }

    fn reweigh(&mut self, path: &Path) {
        let Some((old_bytes, old_tick, new_bytes)) = self.entries.get(path).map(|entry| {
            (
                entry.bytes,
                entry.tick,
                estimate(path, &entry.push, &entry.pull),
            )
        }) else {
            return;
        };
        let tick = self.next_tick();
        {
            let entry = self.entries.get_mut(path).expect("checked above");
            self.bytes = self
                .bytes
                .saturating_sub(old_bytes)
                .saturating_add(new_bytes);
            entry.bytes = new_bytes;
            entry.tick = tick;
        }
        self.order.remove(&old_tick);
        self.order.insert(tick, path.to_path_buf());
        self.evict();
    }

    fn evict(&mut self) {
        while self.entries.len() > self.max_paths
            || (self.bytes > self.max_bytes && !self.is_empty())
        {
            let Some((_, victim)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }
}

fn estimate(path: &Path, push: &[Value], pull: &[Value]) -> usize {
    let push_len = serde_json::to_vec(push).map_or(0, |bytes| bytes.len());
    let pull_len = serde_json::to_vec(pull).map_or(0, |bytes| bytes.len());
    path.as_os_str().len() + push_len + pull_len + std::mem::size_of::<Entry>()
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diagnostic(message: &str) -> Value {
        json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "severity": 1,
            "message": message,
        })
    }

    #[test]
    fn stale_versions_are_ignored() {
        let mut cache = DiagnosticsCache::default();
        let path = Path::new("/a.rs");
        assert!(cache.put_push(path, Some(3), vec![diagnostic("new")]));
        assert!(!cache.put_push(path, Some(2), vec![diagnostic("old")]));
        assert_eq!(
            cache.peek(path).unwrap().push[0]["message"],
            Value::from("new")
        );
    }

    #[test]
    fn evicts_least_recently_used_paths() {
        let mut cache = DiagnosticsCache::new(2, 1024 * 1024);
        let a = Path::new("/a.rs");
        let b = Path::new("/b.rs");
        let c = Path::new("/c.rs");
        cache.put_push(a, None, vec![diagnostic("a")]);
        cache.put_push(b, None, vec![diagnostic("b")]);
        // Touch `a` so `b` becomes the LRU victim.
        let _ = cache.get(a);
        cache.put_push(c, None, vec![diagnostic("c")]);
        assert!(cache.peek(a).is_some());
        assert!(cache.peek(b).is_none());
        assert!(cache.peek(c).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evicts_by_byte_budget() {
        let mut cache = DiagnosticsCache::new(100, 2_400);
        for index in 0..20 {
            let path = PathBuf::from(format!("/file-{index}.rs"));
            cache.put_push(&path, None, vec![diagnostic(&"x".repeat(200))]);
        }
        assert!(cache.bytes() <= 2_400, "bytes={}", cache.bytes());
        assert!(cache.len() < 20);
    }

    #[test]
    fn pull_and_push_merge_without_duplicates() {
        let mut cache = DiagnosticsCache::default();
        let path = Path::new("/a.rs");
        cache.put_push(path, Some(1), vec![diagnostic("dup")]);
        cache.put_pull(
            path,
            vec![diagnostic("dup"), diagnostic("pull")],
            Some("r1".into()),
        );
        let entry = cache.get(path).unwrap();
        assert_eq!(entry.merged().len(), 2);
        assert_eq!(entry.errors().len(), 2);
        assert_eq!(entry.result_id.as_deref(), Some("r1"));
    }

    #[test]
    fn remove_and_clear_release_bytes() {
        let mut cache = DiagnosticsCache::default();
        cache.put_push(Path::new("/a.rs"), None, vec![diagnostic("a")]);
        assert!(cache.bytes() > 0);
        cache.remove(Path::new("/a.rs"));
        assert_eq!(cache.bytes(), 0);
        cache.put_push(Path::new("/b.rs"), None, vec![diagnostic("b")]);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.bytes(), 0);
    }
}
