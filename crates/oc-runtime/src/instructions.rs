//! Hash-frozen instruction sources and their deltas.
//!
//! Order is most-shared → least-shared so the provider prompt cache prefix
//! stays stable:
//! code-mode → MCP → references → skills → discovery → builtins → memory →
//! goal → session.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Canonical source ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    CodeMode,
    Mcp,
    References,
    Skills,
    Discovery,
    Builtins,
    Memory,
    Boxes,
    Goal,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSource {
    pub kind: SourceKind,
    pub key: String,
    pub text: String,
}

impl InstructionSource {
    #[must_use]
    pub fn new(kind: SourceKind, key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
            text: text.into(),
        }
    }

    #[must_use]
    pub fn hash(&self) -> String {
        hash_text(&self.text)
    }
}

/// Stable short hash used in `session.instructions.updated`.
#[must_use]
pub fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(&digest[..8])
}

/// Render sources in canonical order. Empty sources are skipped; parts are
/// joined with a blank line (upstream `"\n\n"`).
#[must_use]
pub fn render(sources: &[InstructionSource]) -> String {
    let mut sorted: Vec<&InstructionSource> = sources
        .iter()
        .filter(|source| !source.text.trim().is_empty())
        .collect();
    sorted.sort_by(|left, right| {
        (left.kind, left.key.as_str()).cmp(&(right.kind, right.key.as_str()))
    });
    sorted
        .iter()
        .map(|source| source.text.trim_end())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The frozen instruction prefix plus per-key hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSet {
    initial: String,
    hashes: BTreeMap<String, String>,
    sources: BTreeMap<String, InstructionSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstructionDelta {
    /// Changed keys → new hash; removed keys → `"removed"`.
    pub delta: BTreeMap<String, String>,
    /// Full new prefix when anything changed.
    pub text: Option<String>,
}

impl InstructionSet {
    #[must_use]
    pub fn new(sources: &[InstructionSource]) -> Self {
        Self {
            initial: render(sources),
            hashes: sources
                .iter()
                .map(|source| (source.key.clone(), source.hash()))
                .collect(),
            sources: sources
                .iter()
                .map(|source| (source.key.clone(), source.clone()))
                .collect(),
        }
    }

    #[must_use]
    pub fn initial(&self) -> &str {
        &self.initial
    }

    #[must_use]
    pub fn hashes(&self) -> &BTreeMap<String, String> {
        &self.hashes
    }

    #[must_use]
    pub fn source(&self, key: &str) -> Option<&InstructionSource> {
        self.sources.get(key)
    }

    /// Stabilize volatile sources (`date`, an already-established `oc/memory`
    /// block, and `oc/goal`) within an active epoch so UTC midnight rollovers,
    /// per-turn FTS query churn, or goal progress updates never invalidate the
    /// provider prompt cache.
    ///
    /// The memory pin holds while every memory ID it rendered is still live in
    /// the store (see [`Self::memory_pin_holds_with_live_ids`]). Once a memory
    /// is forgotten, the freshly assembled block replaces the pin so the cached
    /// prefix stops re-sending it; the next turn pins that fresh block again,
    /// so the prefix changes once per membership deletion, not once per turn.
    #[must_use]
    pub fn stabilize_epoch_sources(&self, next: &[InstructionSource]) -> Vec<InstructionSource> {
        self.stabilize_epoch_sources_with(next, self.memory_pin_holds(next))
    }

    /// Stabilize volatile sources with an explicit `pin_memory` decision.
    #[must_use]
    pub fn stabilize_epoch_sources_with(
        &self,
        next: &[InstructionSource],
        pin_memory: bool,
    ) -> Vec<InstructionSource> {
        let mut out: Vec<InstructionSource> = Vec::with_capacity(next.len() + 2);
        for source in next {
            if source.key == "date"
                && let Some(pinned) = self.sources.get("date")
            {
                out.push(pinned.clone());
                continue;
            }
            if source.key == "oc/memory" {
                if pin_memory {
                    if let Some(pinned) = self.sources.get("oc/memory") {
                        out.push(pinned.clone());
                    } else {
                        out.push(source.clone());
                    }
                } else {
                    out.push(source.clone());
                }
                continue;
            }
            if source.key == "oc/goal" || source.key == "oc/boxes" {
                if let Some(pinned) = self.sources.get(&source.key) {
                    out.push(pinned.clone());
                }
                // When the epoch started without the source, keep it out of the
                // system prefix and deliver it via `epoch_supplement` so a
                // mid-session change does not bust the prompt cache.
                continue;
            }
            out.push(source.clone());
        }
        if pin_memory
            && let Some(pinned_memory) = self.sources.get("oc/memory")
            && !out.iter().any(|source| source.key == "oc/memory")
        {
            out.push(pinned_memory.clone());
        }
        for key in ["oc/boxes", "oc/goal"] {
            if let Some(pinned) = self.sources.get(key)
                && !out.iter().any(|source| source.key == key)
            {
                out.push(pinned.clone());
            }
        }
        out.sort_by(|left, right| {
            (left.kind, left.key.as_str()).cmp(&(right.kind, right.key.as_str()))
        });
        out
    }

    /// Whether the pinned `oc/memory` block may be carried forward based solely
    /// on the freshly rendered sources.
    #[must_use]
    pub fn memory_pin_holds(&self, next: &[InstructionSource]) -> bool {
        let Some(pinned) = self.sources.get("oc/memory") else {
            return true;
        };
        let Some(candidate) = next.iter().find(|source| source.key == "oc/memory") else {
            return false;
        };
        let fresh: BTreeSet<&str> = memory_lines(&candidate.text).collect();
        memory_lines(&pinned.text).all(|line| fresh.contains(line))
    }

    /// Whether the pinned `oc/memory` block may be carried forward when
    /// `live_ids` is the set of non-deleted memory IDs in the store.
    ///
    /// A memory that merely fell out of the top-8 FTS ranking on a new user
    /// prompt is still live in the store, so keeping the pin avoids mutating
    /// the 80k-token system prefix while novel hits are delivered at the tail.
    #[must_use]
    pub fn memory_pin_holds_with_live_ids(
        &self,
        next: &[InstructionSource],
        live_ids: &BTreeSet<String>,
    ) -> bool {
        if self.memory_pin_holds(next) {
            return true;
        }
        let Some(pinned) = self.sources.get("oc/memory") else {
            return true;
        };
        let ids: Vec<&str> = memory_lines(&pinned.text)
            .filter_map(memory_line_id)
            .collect();
        !ids.is_empty() && ids.into_iter().all(|id| live_ids.contains(id))
    }

    /// Extract any newly retrieved memory lines in `next` that are not already
    /// present in the epoch's pinned `oc/memory` source, so they can be
    /// injected at the conversation tail without mutating the system prefix.
    #[must_use]
    pub fn memory_supplement(&self, next: &[InstructionSource]) -> Option<String> {
        self.epoch_supplement(next, true)
    }

    /// Extract tail supplements for `oc/memory` and `oc/goal` changes within an
    /// active epoch so the model sees newly retrieved memories and updated goal
    /// state without mutating the cached system prefix.
    #[must_use]
    pub fn epoch_supplement(&self, next: &[InstructionSource], pin_memory: bool) -> Option<String> {
        let mut blocks = Vec::new();
        if pin_memory
            && let Some(pinned) = self.sources.get("oc/memory")
            && let Some(candidate) = next.iter().find(|source| source.key == "oc/memory")
            && candidate.text != pinned.text
        {
            let mut novel = Vec::new();
            for line in candidate.text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("- [") && !pinned.text.contains(trimmed) {
                    novel.push(trimmed);
                }
            }
            if !novel.is_empty() {
                blocks.push(format!(
                    "<memory scope=\"supplement\">\n{}\n</memory>",
                    novel.join("\n")
                ));
            }
        }
        for key in ["oc/boxes", "oc/goal"] {
            if let Some(candidate) = next.iter().find(|source| source.key == key) {
                let changed = self
                    .sources
                    .get(key)
                    .is_none_or(|pinned| pinned.text != candidate.text);
                if changed {
                    blocks.push(candidate.text.trim().to_owned());
                }
            }
        }
        if blocks.is_empty() {
            None
        } else {
            Some(format!(
                "<system-reminder>\n{}\n</system-reminder>",
                blocks.join("\n\n")
            ))
        }
    }

    /// Compute the delta to `next` without mutating self.
    #[must_use]
    pub fn delta(&self, next: &[InstructionSource]) -> InstructionDelta {
        let mut delta = BTreeMap::new();
        let mut next_hashes = BTreeMap::new();

        for source in next {
            let hash = source.hash();
            next_hashes.insert(source.key.clone(), hash.clone());
            if self.hashes.get(&source.key) != Some(&hash) {
                delta.insert(source.key.clone(), hash);
            }
        }
        for key in self.hashes.keys() {
            if !next_hashes.contains_key(key) {
                delta.insert(key.clone(), "removed".to_owned());
            }
        }

        let text = (!delta.is_empty()).then(|| render(next));
        InstructionDelta { delta, text }
    }

    /// Apply a delta and return it.
    pub fn commit(&mut self, next: &[InstructionSource]) -> InstructionDelta {
        let delta = self.delta(next);
        if !delta.delta.is_empty() {
            self.initial = render(next);
            self.hashes = next
                .iter()
                .map(|source| (source.key.clone(), source.hash()))
                .collect();
            self.sources = next
                .iter()
                .map(|source| (source.key.clone(), source.clone()))
                .collect();
        }
        delta
    }
}

/// Every `- [...]` line rendered in a memory block, trimmed for comparison.
///
/// Rendering is the only thing both the pin and a fresh assembly share: the
/// line carries the id in parentheses (`- [fact] text (mem_…)`), so comparing
/// rendered lines detects a forgotten memory without re-reading the store.
fn memory_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- ["))
}

fn memory_line_id(line: &str) -> Option<&str> {
    let open = line.rfind(" (")?;
    let rest = &line[open + 2..];
    let id = rest.strip_suffix(')')?;
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(kind: SourceKind, key: &str, text: &str) -> InstructionSource {
        InstructionSource::new(kind, key, text)
    }

    #[test]
    fn renders_in_canonical_order() {
        let rendered = render(&[
            source(SourceKind::Session, "session/a", "SESSION"),
            source(SourceKind::CodeMode, "core/codemode", "CODEMODE"),
            source(SourceKind::Skills, "core/skill-guidance", "SKILLS"),
            source(SourceKind::Builtins, "core/builtins", "BUILTINS"),
        ]);
        assert_eq!(rendered, "CODEMODE\n\nSKILLS\n\nBUILTINS\n\nSESSION");
    }

    #[test]
    fn empty_sources_are_skipped() {
        assert_eq!(
            render(&[
                source(SourceKind::Mcp, "mcp", "  "),
                source(SourceKind::Discovery, "discovery", "AGENTS"),
            ]),
            "AGENTS"
        );
    }

    #[test]
    fn delta_reports_added_changed_and_removed() {
        let mut set = InstructionSet::new(&[
            source(SourceKind::Builtins, "builtins", "date"),
            source(SourceKind::Skills, "skills", "old"),
        ]);
        let delta = set.commit(&[
            source(SourceKind::Builtins, "builtins", "date"),
            source(SourceKind::Skills, "skills", "new"),
            source(SourceKind::Memory, "memory", "remember"),
        ]);
        assert_eq!(delta.delta.len(), 2);
        assert_eq!(delta.delta["memory"], hash_text("remember"));
        assert_eq!(delta.delta["skills"], hash_text("new"));
        assert!(!delta.delta.contains_key("builtins"));
        assert_eq!(delta.text.as_deref(), Some("new\n\ndate\n\nremember"));

        let removed = set.commit(&[source(SourceKind::Builtins, "builtins", "date")]);
        assert_eq!(removed.delta["memory"], "removed");
        assert_eq!(removed.delta["skills"], "removed");
        assert_eq!(set.initial(), "date");
    }

    #[test]
    fn unchanged_sources_produce_no_delta() {
        let sources = [source(SourceKind::Builtins, "builtins", "date")];
        let mut set = InstructionSet::new(&sources);
        let delta = set.commit(&sources);
        assert!(delta.delta.is_empty());
        assert!(delta.text.is_none());
    }

    #[test]
    fn hash_is_stable_and_short() {
        assert_eq!(hash_text("abc"), hash_text("abc"));
        assert_ne!(hash_text("abc"), hash_text("abd"));
        assert_eq!(hash_text("abc").len(), 16);
    }

    #[test]
    fn stabilize_epoch_sources_pins_date_and_memory_and_extracts_supplement() {
        let initial = [
            source(
                SourceKind::Builtins,
                "date",
                "Today's date is 2026-04-17 (UTC).",
            ),
            source(
                SourceKind::Memory,
                "oc/memory",
                "<memory scope=\"project\">\n- [fact] alpha (mem_1)\n</memory>",
            ),
        ];
        let mut set = InstructionSet::new(&initial);
        let next = [
            source(
                SourceKind::Builtins,
                "date",
                "Today's date is 2026-04-18 (UTC).",
            ),
            source(
                SourceKind::Memory,
                "oc/memory",
                "<memory scope=\"project\">\n- [fact] alpha (mem_1)\n- [fact] beta (mem_2)\n</memory>",
            ),
        ];
        let supplement = set.memory_supplement(&next).expect("novel memory line");
        assert!(supplement.contains("- [fact] beta (mem_2)"));
        assert!(!supplement.contains("alpha"));

        let stabilized = set.stabilize_epoch_sources(&next);
        let delta = set.commit(&stabilized);
        assert!(
            delta.delta.is_empty(),
            "date rollover and memory churn must not mutate the cached prefix"
        );
    }
}
