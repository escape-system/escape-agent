//! Rendering helpers: token estimates, near-duplicate detection and the
//! `<memory>` instruction block.
//!
//! Token counts are the local `chars / 4` estimate used everywhere memory is
//! injected; the crate deliberately does not depend on `oc-runtime` or a
//! tokenizer.

use std::collections::BTreeSet;

use crate::service::MemoryBlock;

/// Default number of estimated tokens in an injected memory block.
pub const DEFAULT_TOKEN_BUDGET: usize = 800;

/// Characters per estimated token.
pub const CHARS_PER_TOKEN: usize = 4;

/// Near-duplicate threshold expressed as `17/20 = 0.85`.
const JACCARD_DENOMINATOR: usize = 20;
/// Numerator of the same threshold; integer math avoids float edge cases.
const JACCARD_NUMERATOR: usize = 17;

/// Estimated tokens for `text` (`ceil(chars / 4)`, saturating).
#[must_use]
pub fn estimate_tokens(text: &str) -> u64 {
    u64::try_from(text.chars().count().div_ceil(CHARS_PER_TOKEN)).unwrap_or(u64::MAX)
}

/// Lowercased alphanumeric tokens, used for near-duplicate detection.
#[must_use]
pub fn token_set(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether two token sets have Jaccard similarity `>= 0.85`.
///
/// Two empty sets are treated as duplicates (`1.0`); an empty set against a
/// non-empty one is not.
#[must_use]
pub fn near_duplicate(left: &BTreeSet<String>, right: &BTreeSet<String>) -> bool {
    if left.is_empty() || right.is_empty() {
        return left.is_empty() && right.is_empty();
    }
    let union = left.union(right).count();
    let intersection = left.intersection(right).count();
    intersection.saturating_mul(JACCARD_DENOMINATOR) >= union.saturating_mul(JACCARD_NUMERATOR)
}

/// Collapses all whitespace so one memory always renders on one line.
pub(crate) fn normalize_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Renders `lines` as a `<memory>` block that fits `token_budget`.
///
/// `lines` are `(id, rendered line)` already ordered best-first. A line that
/// does not fit ends the block (never cuts mid-line); when no line fits the
/// block is empty.
pub(crate) fn render_block(
    scope: &str,
    lines: &[(String, String)],
    token_budget: usize,
) -> MemoryBlock {
    let open = format!("<memory scope=\"{scope}\">");
    let close = "</memory>";
    let budget_chars = token_budget.max(1).saturating_mul(CHARS_PER_TOKEN);
    let wrapper_chars = open.chars().count() + 1 + close.chars().count();

    let mut body = String::new();
    let mut ids = Vec::new();
    let mut used = wrapper_chars;
    for (id, line) in lines {
        let cost = line.chars().count() + 1;
        if used.saturating_add(cost) > budget_chars {
            break;
        }
        body.push('\n');
        body.push_str(line);
        used += cost;
        ids.push(id.clone());
    }

    if ids.is_empty() {
        return MemoryBlock::default();
    }

    let text = format!("{open}{body}\n{close}");
    let tokens = estimate_tokens(&text);
    MemoryBlock { text, ids, tokens }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    #[test]
    fn jaccard_threshold_matches_documented_value() {
        let base = token_set("alpha beta gamma delta epsilon zeta eta theta iota kappa");
        let nine_of_ten = token_set("alpha beta gamma delta epsilon zeta eta theta iota");
        let eight_of_ten = token_set("alpha beta gamma delta epsilon zeta eta theta");
        assert!(near_duplicate(&base, &nine_of_ten));
        assert!(!near_duplicate(&base, &eight_of_ten));
        assert!(near_duplicate(&BTreeSet::new(), &BTreeSet::new()));
        assert!(!near_duplicate(&BTreeSet::new(), &base));
    }

    #[test]
    fn block_stops_before_a_line_that_does_not_fit() {
        let lines = vec![
            ("mem_a".to_owned(), "- [fact] short line (mem_a)".to_owned()),
            (
                "mem_b".to_owned(),
                "- [fact] another line (mem_b)".to_owned(),
            ),
        ];
        let full = render_block("project", &lines, DEFAULT_TOKEN_BUDGET);
        assert_eq!(full.ids.len(), 2);
        assert!(
            full.text
                .starts_with("<memory scope=\"project\">\n- [fact] short line")
        );
        assert!(full.text.ends_with("</memory>"));

        let tiny = render_block("project", &lines, 1);
        assert_eq!(tiny, MemoryBlock::default());
        assert!(tiny.text.is_empty());
    }
}
