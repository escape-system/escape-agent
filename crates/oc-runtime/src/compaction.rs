//! Compaction summary contract.

/// Headings the summary must contain, in order.
pub const SUMMARY_SECTIONS: [&str; 7] = [
    "Objective",
    "Requirements",
    "Decisions",
    "Work State",
    "Next Move",
    "Relevant Files",
    "Important Context",
];

/// Instruction block used for a compaction request. The model is asked to
/// replace history with this summary, keeping only the tail.
pub const SUMMARY_INSTRUCTION: &str = "\
Compress the conversation above into a handoff summary for the next step. \
Keep facts, decisions, file paths and failing test names. Drop pleasantries, \
duplicated tool output and speculation. Use exactly these sections, in order, \
with these headings:\n\
## Objective\n\
## Requirements\n\
## Decisions\n\
## Work State\n\
## Next Move\n\
## Relevant Files\n\
## Important Context\n";

/// Default compaction settings.
///
/// `auto` is **false**: threshold compaction runs only when the document opts
/// in. Manual `compaction` inbox rows, the `compact_context` tool and provider
/// overflow recovery remain available regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub auto: bool,
    pub keep_tokens: u64,
    pub buffer: u64,
    /// Optional ceiling override. It may only lower the computed ceiling
    /// (`context_window − max(15%, 16k)`), never raise it.
    pub ceiling_tokens: Option<u64>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: false,
            keep_tokens: 15_000,
            buffer: 0,
            ceiling_tokens: None,
        }
    }
}

impl CompactionConfig {
    /// Read `compaction` from the merged config document.
    ///
    /// Unknown or invalid values fall back to the default: the normalizer
    /// already rejected them at load time, so this only guards against a
    /// hand-edited `merged()` value.
    #[must_use]
    pub fn from_merged(merged: &serde_json::Value) -> Self {
        let default = Self::default();
        let Some(object) = merged
            .get("compaction")
            .and_then(serde_json::Value::as_object)
        else {
            return default;
        };
        Self {
            auto: object
                .get("auto")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(default.auto),
            keep_tokens: object
                .get("keep")
                .and_then(serde_json::Value::as_object)
                .and_then(|keep| keep.get("tokens"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(default.keep_tokens),
            buffer: object
                .get("buffer")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(default.buffer),
            ceiling_tokens: object
                .get("ceiling_tokens")
                .and_then(serde_json::Value::as_u64)
                .filter(|tokens| *tokens > 0),
        }
    }
}

/// Build the compaction prompt, optionally nudging a missing section.
#[must_use]
pub fn summary_prompt(missing_section: Option<&str>) -> String {
    match missing_section {
        Some(section) => format!(
            "{SUMMARY_INSTRUCTION}\nYour previous summary omitted `## {section}`. Rewrite the full \
             summary and include every section."
        ),
        None => SUMMARY_INSTRUCTION.to_owned(),
    }
}

/// Parse which required sections are present, in order.
#[must_use]
pub fn missing_sections(summary: &str) -> Vec<&'static str> {
    SUMMARY_SECTIONS
        .iter()
        .copied()
        .filter(|section| !summary.contains(&format!("## {section}")))
        .collect()
}

/// `keepID` identifies the oldest message preserved verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub summary: String,
    pub keep_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_lists_every_section() {
        for section in SUMMARY_SECTIONS {
            assert!(summary_prompt(None).contains(&format!("## {section}")));
        }
    }

    #[test]
    fn missing_sections_are_detected_in_order() {
        let summary = "## Objective\nx\n## Decisions\ny\n## Next Move\nz".to_owned();
        assert_eq!(
            missing_sections(&summary),
            vec![
                "Requirements",
                "Work State",
                "Relevant Files",
                "Important Context"
            ]
        );
    }

    #[test]
    fn complete_summary_has_no_missing_sections() {
        let summary = SUMMARY_SECTIONS
            .iter()
            .map(|section| format!("## {section}\ncontent"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(missing_sections(&summary).is_empty());
    }

    #[test]
    fn retry_prompt_names_the_missing_section() {
        assert!(summary_prompt(Some("Work State")).contains("Work State"));
    }
}
