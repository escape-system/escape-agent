use std::collections::VecDeque;

use oc_subagents::{Limits, SiblingBudget, SpawnBudget};

use super::digest::DIGEST_CACHE_CAPACITY;

/// Per-runtime mutable tool state: rate budgets and the digest cache.
#[derive(Debug, Default)]
pub(crate) struct SubagentState {
    pub(super) spawn: std::collections::HashMap<String, SpawnBudget>,
    pub(super) siblings: std::collections::HashMap<String, SiblingBudget>,
    pub(super) digests: DigestCache,
    pub(super) digest_rate: std::collections::HashMap<String, VecDeque<i64>>,
}

impl SubagentState {
    pub(super) fn spawn_budget(
        &mut self,
        session: &str,
        limits: &Limits,
        current_children: u32,
    ) -> &mut SpawnBudget {
        self.spawn.entry(session.to_owned()).or_insert_with(|| {
            SpawnBudget::with_default_rate(limits.max_children.saturating_sub(current_children))
        })
    }

    pub(super) fn sibling_budget(&mut self, session: &str, limits: &Limits) -> &mut SiblingBudget {
        self.siblings
            .entry(session.to_owned())
            .or_insert_with(|| SiblingBudget::from_limits(limits))
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
