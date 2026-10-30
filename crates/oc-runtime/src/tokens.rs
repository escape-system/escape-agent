//! Token estimation and compaction thresholds.
//!
//! Upstream is anchor-based: take the last assistant message with measured
//! token usage for the same provider and estimate only the messages after it.
//! Approximations: ~4 characters per token, image 1500, PDF 2000.

/// Estimated tokens for an image part.
pub const IMAGE_TOKENS: u64 = 1500;
/// Estimated tokens for a PDF part.
pub const PDF_TOKENS: u64 = 2000;
/// Context reserve floor.
pub const MIN_RESERVE: u64 = 16_000;

/// ~4 characters per token, rounded up.
#[must_use]
pub fn estimate_text(text: &str) -> u64 {
    let chars = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
    chars.div_ceil(4)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptEstimate {
    pub measured: u64,
    pub estimated: u64,
}

impl PromptEstimate {
    #[must_use]
    pub fn total(self) -> u64 {
        self.measured.saturating_add(self.estimated)
    }
}

/// Estimate the prompt given a measured anchor for everything up to a point.
#[must_use]
pub fn estimate_after_anchor(anchor: Option<u64>, messages_after: &[u64]) -> PromptEstimate {
    PromptEstimate {
        measured: anchor.unwrap_or(0),
        estimated: messages_after.iter().sum(),
    }
}

/// Reserve for the next output + protocol overhead: `max(15%, 16k)`.
#[must_use]
pub fn reserve(context_window: u64) -> u64 {
    (context_window / 100 * 15).max(MIN_RESERVE)
}

/// Context ceiling before compaction triggers.
#[must_use]
pub fn compaction_ceiling(context_window: u64) -> u64 {
    context_window.saturating_sub(reserve(context_window))
}

/// Precompute compaction at 85% of the ceiling.
#[must_use]
pub fn precompute_threshold(ceiling: u64) -> u64 {
    ceiling / 100 * 85
}

/// True when compaction should run now.
#[must_use]
pub fn should_compact(prompt_tokens: u64, context_window: u64) -> bool {
    prompt_tokens > compaction_ceiling(context_window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_estimation_rounds_up() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("a"), 1);
        assert_eq!(estimate_text("abcd"), 1);
        assert_eq!(estimate_text("abcde"), 2);
    }

    #[test]
    fn unicode_counts_characters_not_bytes() {
        assert_eq!(estimate_text("çğü"), 1);
        assert_eq!(estimate_text("çğüş"), 1);
    }

    #[test]
    fn reserve_is_fifteen_percent_or_16k() {
        assert_eq!(reserve(100_000), 16_000);
        assert_eq!(reserve(200_000), 30_000);
        assert_eq!(reserve(10_000), 16_000);
    }

    #[test]
    fn ceiling_and_precompute() {
        assert_eq!(compaction_ceiling(200_000), 170_000);
        assert_eq!(precompute_threshold(170_000), 144_500);
        assert!(should_compact(170_001, 200_000));
        assert!(!should_compact(170_000, 200_000));
    }

    #[test]
    fn anchor_adds_measured_and_estimated() {
        let estimate = estimate_after_anchor(Some(50_000), &[100, 200]);
        assert_eq!(estimate.measured, 50_000);
        assert_eq!(estimate.estimated, 300);
        assert_eq!(estimate.total(), 50_300);
    }

    #[test]
    fn tiny_window_does_not_underflow() {
        assert_eq!(compaction_ceiling(10_000), 0);
    }
}
