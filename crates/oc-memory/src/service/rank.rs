//! Ranking and deduplication for retrieval hits: pinned first, then
//! preferences, then hybrid score; exact ids and near duplicates keep the
//! higher-ranked entry.

use std::collections::{BTreeSet, HashSet};

use oc_store::MemoryHit;

use crate::render::{near_duplicate, token_set};

use super::PINNED_IMPORTANCE;

fn pinned(hit: &MemoryHit) -> u8 {
    u8::from(hit.importance >= PINNED_IMPORTANCE)
}

fn preference(hit: &MemoryHit) -> u8 {
    u8::from(hit.kind == "preference")
}

/// Pinned first, then preferences, then hybrid score, then importance; exact ids and near
/// duplicates (Jaccard `>= 0.85`) keep the higher-ranked entry.
pub(super) fn rank_and_dedupe(mut hits: Vec<MemoryHit>) -> Vec<MemoryHit> {
    hits.sort_by(|left, right| {
        pinned(right)
            .cmp(&pinned(left))
            .then_with(|| preference(right).cmp(&preference(left)))
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| right.importance.cmp(&left.importance))
    });

    let mut seen = HashSet::with_capacity(hits.len());
    hits.retain(|hit| seen.insert(hit.id.clone()));

    let mut kept: Vec<MemoryHit> = Vec::with_capacity(hits.len());
    let mut kept_tokens: Vec<BTreeSet<String>> = Vec::new();
    for hit in hits {
        let tokens = token_set(&hit.text);
        if kept_tokens
            .iter()
            .any(|existing| near_duplicate(existing, &tokens))
        {
            continue;
        }
        kept_tokens.push(tokens);
        kept.push(hit);
    }
    kept
}
