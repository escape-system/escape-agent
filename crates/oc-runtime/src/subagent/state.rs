use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use oc_subagents::{Limits, SiblingBudget, SpawnBudget, WriteBudget};

use super::digest::DIGEST_CACHE_CAPACITY;
use super::settings::SubagentSettings;

/// Maximum lifetime of an armed streaming subagent caller pre-warm entry.
pub(super) const PREWARM_TTL_MS: i64 = 10_000;

/// Pre-loaded caller metadata computed concurrently while `subagent` JSON args stream.
#[derive(Clone, Debug)]
pub(crate) struct WarmCallerData {
    pub(crate) parent: oc_subagents::SessionNode,
    pub(crate) parent_info: oc_types::SessionInfo,
    pub(crate) settings: SubagentSettings,
    pub(crate) available_agents: Vec<String>,
    pub(crate) primary_only: Vec<String>,
    pub(crate) quota_usage: Option<Vec<oc_store::ModelUsageRow>>,
}

#[derive(Clone, Debug)]
pub(super) struct WarmCallerSlot {
    pub(super) created_ms: i64,
    pub(super) cell: Arc<tokio::sync::OnceCell<Arc<WarmCallerData>>>,
}

/// Per-runtime mutable tool state: rate budgets, active watcher set, and the digest cache.
#[derive(Debug, Default)]
pub(crate) struct SubagentState {
    pub(super) spawn: std::collections::HashMap<String, SpawnBudget>,
    pub(super) siblings: std::collections::HashMap<String, SiblingBudget>,
    pub(super) writes: std::collections::HashMap<String, WriteBudget>,
    pub(super) digests: DigestCache,
    pub(super) digest_rate: std::collections::HashMap<String, VecDeque<i64>>,
    pub(super) active_watchers: HashSet<(String, String)>,
    pub(super) seen_queue_inbox_ids: std::collections::HashMap<String, HashSet<String>>,
    pub(super) warm_callers: std::collections::HashMap<String, WarmCallerSlot>,
}

impl SubagentState {
    pub(super) fn spawn_budget(
        &mut self,
        session: &str,
        limits: &Limits,
        current_children: u32,
    ) -> &mut SpawnBudget {
        let rate = limits.max_children.clamp(16, 64);
        let budget = self
            .spawn
            .entry(session.to_owned())
            .or_insert_with(|| SpawnBudget::new(limits.max_children, rate));
        budget.reconfigure(limits.max_children, rate);
        budget.sync_active(current_children);
        budget
    }

    pub(super) fn sibling_budget(&mut self, session: &str, limits: &Limits) -> &mut SiblingBudget {
        self.siblings
            .entry(session.to_owned())
            .or_insert_with(|| SiblingBudget::from_limits(limits))
    }

    pub(super) fn write_budget(&mut self, session: &str, limits: &Limits) -> &mut WriteBudget {
        self.writes
            .entry(session.to_owned())
            .or_insert_with(|| WriteBudget::from_limits(limits))
    }

    pub(super) fn has_unseen_queue_ids(&self, session: &str, ids: &[String]) -> bool {
        let Some(seen) = self.seen_queue_inbox_ids.get(session) else {
            return !ids.is_empty();
        };
        ids.iter().any(|id| !seen.contains(id))
    }

    pub(super) fn mark_queue_ids_seen(&mut self, session: &str, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let seen = self
            .seen_queue_inbox_ids
            .entry(session.to_owned())
            .or_default();
        for id in ids {
            seen.insert(id.clone());
        }
    }

    pub(crate) fn clear_queue_ids_seen(&mut self, session: &str) {
        self.seen_queue_inbox_ids.remove(session);
    }
}

#[derive(Debug, Default)]
pub(super) struct DigestCache {
    pub(super) entries: VecDeque<(String, String)>,
}

impl DigestCache {
    pub(super) fn get(&mut self, key: &str) -> Option<String> {
        let index = self.entries.iter().position(|(stored, _)| stored == key)?;
        let entry = self.entries.remove(index)?;
        let value = entry.1.clone();
        self.entries.push_back(entry);
        Some(value)
    }

    pub(super) fn put(&mut self, key: String, value: String) {
        if let Some(index) = self.entries.iter().position(|(stored, _)| stored == &key) {
            self.entries.remove(index);
        }
        while self.entries.len() >= DIGEST_CACHE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }
}
