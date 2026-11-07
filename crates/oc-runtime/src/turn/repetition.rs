//! Repetition Guard: detects degenerate model token repetition loops (Hermes `repetition_guard` port).
//!
//! A model in a degenerate repetition loop can spend its entire output budget echoing
//! one fragment. Naive length continuations then endlessly re-request continuation,
//! corrupting context and burning tokens. This guard detects repetition-dominated
//! text before continuation.

use std::collections::HashMap;

const MIN_FRAGMENT_LENGTH: usize = 400;
const REPEAT_WINDOW: usize = 60;
const MIN_REPEAT_COUNT: usize = 4;

/// Returns true when a contiguous run or set of repeating segments covers at least half of `text`.
#[must_use]
pub fn is_repetition_dominated(text: &str) -> bool {
    let char_count = text.chars().count();
    if char_count < MIN_FRAGMENT_LENGTH {
        return false;
    }

    if line_repetition_dominated(text, char_count) {
        return true;
    }

    window_repetition_dominated(text, char_count)
}

/// Checks if repeated lines dominate at least 50% of the character count.
fn line_repetition_dominated(text: &str, total_chars: usize) -> bool {
    let mut line_counts: HashMap<&str, usize> = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.len() >= 10 {
            *line_counts.entry(trimmed).or_insert(0) += 1;
        }
    }

    let threshold = total_chars / 2;
    for (line, count) in line_counts {
        if count >= MIN_REPEAT_COUNT {
            let contribution = line.chars().count() * count;
            if contribution >= threshold {
                return true;
            }
        }
    }

    false
}

/// Checks if a sliding 60-character window repeats frequently enough to cover >= 50% of the text.
fn window_repetition_dominated(text: &str, total_chars: usize) -> bool {
    if total_chars < REPEAT_WINDOW * MIN_REPEAT_COUNT {
        return false;
    }

    let chars: Vec<char> = text.chars().collect();
    let mut window_counts: HashMap<String, usize> = HashMap::new();

    // Sample windows every 20 characters to remain linear in runtime
    let step = 20;
    for i in (0..chars.len().saturating_sub(REPEAT_WINDOW)).step_by(step) {
        let window: String = chars[i..i + REPEAT_WINDOW].iter().collect();
        *window_counts.entry(window).or_insert(0) += 1;
    }

    let min_samples = (total_chars / 2) / step;
    for count in window_counts.values() {
        if *count >= MIN_REPEAT_COUNT && *count >= min_samples.max(MIN_REPEAT_COUNT) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_never_dominated() {
        assert!(!is_repetition_dominated(
            "short repeating text short repeating text"
        ));
    }

    #[test]
    fn natural_text_with_slight_repetition_is_not_dominated() {
        let text = "
            In software engineering, testing is critical.
            We write unit tests to verify behavior and avoid regression.
            Integration tests verify that subsystems work together properly.
            Every test should be deterministic, offline, and run quickly.
            Testing is critical, and we must always maintain tests.
        "
        .repeat(10);
        assert!(!is_repetition_dominated(&text));
    }

    #[test]
    fn catches_degenerate_repeating_line() {
        let bad_line = "let value = compute_something_very_important();\n";
        let bad_text = bad_line.repeat(30); // 30 * ~48 chars = ~1440 chars
        assert!(is_repetition_dominated(&bad_text));
    }

    #[test]
    fn catches_degenerate_repeating_paragraph() {
        let para = "The quick brown fox jumps over the lazy dog and runs around the block repeatedly until dawn.\n";
        let text = para.repeat(15);
        assert!(is_repetition_dominated(&text));
    }
}
