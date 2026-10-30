//! Hash-frozen instruction sources and their deltas.
//!
//! Order is most-shared → least-shared so the provider prompt cache prefix
//! stays stable:
//! code-mode → MCP → references → skills → discovery → builtins → memory →
//! goal → session.

use std::collections::BTreeMap;

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
        }
        delta
    }
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
}
