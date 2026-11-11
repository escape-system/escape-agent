//! The terse observer input document and its deterministic truncation.
//!
//! The document contains only: the goal header, root clocks/state, strip
//! `why` + clocks + statuses, recent one-line outcomes, and tree
//! ids/statuses. No message text, no tool arguments, no outputs, no files.
//!
//! Rendering produces compact JSON. When it exceeds the token budget
//! (`max_tokens`, estimated as characters / 4) content is discarded in this
//! order:
//!
//! 1. `recent`, oldest first (the tail of the newest-first list);
//! 2. `tree`, deepest node first;
//! 3. `active`, lowest priority first — overdue strips are never dropped.

use serde::{Deserialize, Serialize};

use crate::registry::StripState;

/// Goal header shown to the observer (a subset of [`crate::GoalSnapshot`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalInput {
    /// Goal id (`gol_…`).
    pub id: String,
    /// One-line objective.
    pub objective: String,
    /// `active`, `paused`, `blocked`, `done`, `failed`, `cancelled`.
    pub status: String,
    /// Criteria lines.
    pub criteria: Vec<String>,
    /// Criteria already satisfied.
    pub done: u32,
    /// Total criteria.
    pub total: u32,
}

/// Root session header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RootInput {
    /// Root session id.
    pub id: String,
    /// Root agent id.
    pub agent: String,
    /// Root model reference; omitted when the host does not know it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Current step number.
    pub step: u32,
    /// `busy`, `idle`, `interrupted`.
    pub state: String,
    /// True when the root was stopped by the user and is still idle as a
    /// result. Omitted when false so the input document stays terse.
    #[serde(rename = "stoppedByUser", skip_serializing_if = "is_false")]
    pub stopped_by_user: bool,
}

/// Serde predicate for `#[serde(skip_serializing_if = ...)]` on bool fields.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde `skip_serializing_if` requires `fn(&T) -> bool`
fn is_false(value: &bool) -> bool {
    !*value
}

/// Optional root metadata the host feeds once per change.
///
/// [`ObserverInput::root`] needs the model and step, which are not derivable
/// from strips or tree rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RootContext {
    /// Root agent id.
    pub agent: Option<String>,
    /// Root model reference.
    pub model: Option<String>,
    /// Current step number.
    pub step: u32,
    /// `busy`, `idle`, `interrupted`.
    pub state: Option<String>,
}

/// One running strip in priority order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveStrip {
    /// Session id.
    pub sid: String,
    /// Agent id.
    pub agent: String,
    /// Tool name.
    pub tool: String,
    /// The only free text.
    pub why: String,
    /// Elapsed seconds.
    pub age_s: i64,
    /// Agent estimate.
    pub expect_s: u32,
    /// Intervention deadline.
    pub intervene_s: u32,
    /// Running state (normally `running`).
    pub state: StripState,
    /// True when `age_s >= intervene_s`.
    pub overdue: bool,
}

/// One settled strip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentStrip {
    /// Tool call id (`tc_…` / `call_…`), omitted when empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Session id.
    pub sid: String,
    /// Tool name.
    pub tool: String,
    /// One-line result; omitted when the tool produced none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Wall time in seconds.
    pub took_s: i64,
    /// The strip's `why`.
    pub why: String,
}

/// One session-tree row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeRow {
    /// Session id.
    pub sid: String,
    /// Parent session id for non-root rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Agent id.
    pub agent: String,
    /// `busy`, `idle`, `failed`, …
    pub state: String,
    /// Durable interruption cause (`user`, `shutdown`, `superseded`,
    /// `inactivity`) when the row stopped as `interrupted`; omitted otherwise.
    ///
    /// The runtime populates this from `session_v2.idle_reason`. The observer
    /// treats `interrupted` + `user` as a user stop and never re-raises that
    /// session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_reason: Option<String>,
    /// Depth from the root (root = 0).
    pub depth: u32,
}

/// The complete observer input document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ObserverInput {
    /// Goal header, when a goal exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalInput>,
    /// Root session header.
    pub root: RootInput,
    /// Running strips, most urgent first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub active: Vec<ActiveStrip>,
    /// Settled strips, newest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<RecentStrip>,
    /// Session tree, root first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tree: Vec<TreeRow>,
}

/// Render the observer input as compact JSON, capped at `max_tokens`.
///
/// Truncation order is documented at module level. The returned string never
/// exceeds `max_tokens * 4` characters unless only overdue active strips
/// remain (they are never dropped).
#[must_use]
pub fn render(input: &ObserverInput, max_tokens: usize) -> String {
    let mut document = input.clone();
    let limit = max_tokens.saturating_mul(4);
    let mut rendered = serialize(&document);

    while char_len(&rendered) > limit {
        if !document.recent.is_empty() {
            document.recent.pop();
            rendered = serialize(&document);
            continue;
        }
        if let Some(index) = deepest_tree_index(&document.tree) {
            document.tree.remove(index);
            rendered = serialize(&document);
            continue;
        }
        if pop_lowest_priority_active(&mut document.active) {
            rendered = serialize(&document);
            continue;
        }
        break;
    }
    rendered
}

fn serialize(document: &ObserverInput) -> String {
    serde_json::to_string(document).unwrap_or_else(|_| "{}".to_owned())
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Deepest row; ties resolve to the latest row so sibling subtrees are
/// trimmed from the end.
fn deepest_tree_index(tree: &[TreeRow]) -> Option<usize> {
    tree.iter()
        .enumerate()
        .max_by_key(|(index, row)| (row.depth, *index))
        .map(|(index, _)| index)
}

/// Drop the lowest-priority non-overdue active strip (the tail of the
/// priority-ordered list). Returns false when every strip is overdue.
fn pop_lowest_priority_active(active: &mut Vec<ActiveStrip>) -> bool {
    let Some(index) = active.iter().rposition(|strip| !strip.overdue) else {
        return false;
    };
    active.remove(index);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active(index: usize) -> ActiveStrip {
        ActiveStrip {
            sid: format!("ses_{index:03}"),
            agent: "general".to_owned(),
            tool: "shell".to_owned(),
            why: "run the focused test suite for the parser".to_owned(),
            age_s: 5,
            expect_s: 90,
            intervene_s: 150,
            state: StripState::Running,
            overdue: false,
        }
    }

    fn base() -> ObserverInput {
        ObserverInput {
            goal: None,
            root: RootInput {
                id: "ses_root".to_owned(),
                agent: "build".to_owned(),
                model: Some("anthropic/claude-sonnet-4-5".to_owned()),
                step: 7,
                state: "busy".to_owned(),
                stopped_by_user: false,
            },
            active: vec![active(0)],
            recent: Vec::new(),
            tree: vec![TreeRow {
                sid: "ses_root".to_owned(),
                parent: None,
                agent: "build".to_owned(),
                state: "busy".to_owned(),
                depth: 0,
                idle_reason: None,
            }],
        }
    }

    fn tokens(document: &ObserverInput) -> usize {
        char_len(&serde_json::to_string(document).expect("serializable")).div_ceil(4)
    }

    #[test]
    fn truncation_drops_recent_then_tree_then_active() {
        // Step 1: only recent needs trimming.
        let mut input = base();
        for index in 0..40 {
            input.recent.push(RecentStrip {
                id: None,
                sid: "ses_child".to_owned(),
                tool: "shell".to_owned(),
                outcome: Some("exit 1".to_owned()),
                took_s: 12,
                why: format!("run focused test {index}"),
            });
        }
        let one_recent = {
            let mut clone = input.clone();
            clone.recent.truncate(1);
            tokens(&clone)
        };
        let rendered = render(&input, one_recent + 2);
        let output: ObserverInput = serde_json::from_str(&rendered).expect("json");
        assert!(!output.recent.is_empty(), "recent trimmed but not empty");
        assert!(output.recent.len() < input.recent.len());
        assert_eq!(output.recent, input.recent[..output.recent.len()]);
        assert_eq!(output.active.len(), 1);
        assert_eq!(output.tree.len(), 1);

        // Step 2: recent is gone, tree loses its deepest rows first.
        let mut input = base();
        input.tree = (0..5)
            .map(|depth| TreeRow {
                sid: format!("ses_t{depth}"),
                parent: Some("ses_root".to_owned()),
                agent: "general".to_owned(),
                state: "busy".to_owned(),
                idle_reason: None,
                depth,
            })
            .collect();
        let three_rows = {
            let mut clone = input.clone();
            clone.tree.truncate(3);
            tokens(&clone)
        };
        let rendered = render(&input, three_rows + 2);
        let output: ObserverInput = serde_json::from_str(&rendered).expect("json");
        assert!(!output.tree.is_empty());
        assert!(output.tree.len() < input.tree.len());
        assert_eq!(output.tree, input.tree[..output.tree.len()]);
        assert!(
            output.active.len() == 1,
            "active untouched while tree shrinks"
        );

        // Step 3: active loses its lowest-priority tail.
        let mut input = base();
        input.active = (0..20).map(active).collect();
        let rendered = render(&input, 80);
        let output: ObserverInput = serde_json::from_str(&rendered).expect("json");
        assert!(!output.active.is_empty());
        assert!(output.active.len() < 20);
        assert_eq!(output.active, input.active[..output.active.len()]);
    }

    #[test]
    fn hundred_strips_respect_the_char_cap() {
        let mut input = base();
        input.active = (0..100).map(active).collect();
        let max_tokens = 600;
        let rendered = render(&input, max_tokens);
        assert!(char_len(&rendered) <= max_tokens * 4);
        let output: ObserverInput = serde_json::from_str(&rendered).expect("json");
        assert!(output.active.len() < 100);
        assert_eq!(output.active[0].sid, "ses_000");
    }

    #[test]
    fn overdue_strips_are_never_dropped() {
        let mut input = base();
        input.active = (0..5)
            .map(|index| {
                let mut strip = active(index);
                strip.overdue = index < 2;
                strip
            })
            .collect();
        let rendered = render(&input, 40);
        let output: ObserverInput = serde_json::from_str(&rendered).expect("json");
        let overdue = output.active.iter().filter(|strip| strip.overdue).count();
        assert_eq!(overdue, 2);
    }
}
