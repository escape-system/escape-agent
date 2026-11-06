//! Strict parsing and validation of the observer verdict.
//!
//! The model must answer with one JSON object (optionally inside a fenced
//! code block tagged `json`). Unknown actions are errors: silently mapping
//! them to `none` would hide a model failure and let a broken prompt look
//! healthy. Unknown *fields* are ignored, as allowed by
//! `docs/08-goals-observer.md` §3.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ObserverConfig;

/// Maximum `reason` characters.
pub const MAX_REASON_CHARS: usize = 160;
/// Maximum `reason` words.
pub const MAX_REASON_WORDS: usize = 20;
/// Maximum repair reason characters (same as `reason`).
pub const MAX_REPAIR_REASON_CHARS: usize = MAX_REASON_CHARS;

/// What the observer wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverAction {
    /// Nothing.
    None,
    /// Inject a synthetic nudge into the target session.
    Nudge,
    /// Run `digest` over the target.
    Inspect,
    /// Apply a repair command.
    Repair,
    /// Mark the goal done (evidence required).
    GoalDone,
    /// Mark the goal blocked.
    GoalBlocked,
    /// Mark the goal failed.
    GoalFailed,
}

impl ObserverAction {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Nudge => "nudge",
            Self::Inspect => "inspect",
            Self::Repair => "repair",
            Self::GoalDone => "goal_done",
            Self::GoalBlocked => "goal_blocked",
            Self::GoalFailed => "goal_failed",
        }
    }

    /// Every action except `none` must name a target.
    #[must_use]
    pub fn requires_target(self) -> bool {
        self != Self::None
    }

    /// Whether this action mutates goal state.
    #[must_use]
    pub fn is_goal(self) -> bool {
        matches!(self, Self::GoalDone | Self::GoalBlocked | Self::GoalFailed)
    }
}

/// Repair command kinds (`docs/08` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairKind {
    /// Re-run a failed tool call. Idempotent tools only.
    RetryTool,
    /// Cancel a hung tool. Never used on `subagent`.
    CancelTool,
    /// Cancel and respawn a child session with the same prompt.
    RestartSubagent,
    /// Force compaction on the target.
    CompactContext,
    /// Switch the target to the escalation model.
    SwitchModel,
    /// Re-deliver the last user prompt.
    Repost,
}

impl RepairKind {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RetryTool => "retry_tool",
            Self::CancelTool => "cancel_tool",
            Self::RestartSubagent => "restart_subagent",
            Self::CompactContext => "compact_context",
            Self::SwitchModel => "switch_model",
            Self::Repost => "repost",
        }
    }
}

/// A repair command with its kind-specific arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Repair {
    /// What to do.
    pub kind: Option<RepairKind>,
    /// Arguments interpreted by the host executor.
    pub args: Value,
}

impl Default for Repair {
    fn default() -> Self {
        Self {
            kind: None,
            args: Value::Object(serde_json::Map::new()),
        }
    }
}

/// A validated observer verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObserverVerdict {
    /// Chosen action.
    pub action: ObserverAction,
    /// Session id or tool-call id the action applies to.
    pub target: Option<String>,
    /// Why the observer intervened (≤ 20 words, ≤ 160 chars).
    pub reason: String,
    /// Nudge text (≤ `nudge_max_chars`).
    pub message: Option<String>,
    /// Repair command, present iff `action == Repair`.
    pub repair: Option<Repair>,
    /// Evidence ids for `goal_done`.
    pub goal_evidence: Vec<String>,
}

impl ObserverVerdict {
    /// A verdict that asks for nothing.
    #[must_use]
    pub fn none(reason: impl Into<String>) -> Self {
        Self {
            action: ObserverAction::None,
            target: None,
            reason: reason.into(),
            message: None,
            repair: None,
            goal_evidence: Vec::new(),
        }
    }
}

/// Verdict validation failures. All are model failures unless noted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerdictError {
    /// Not a JSON object at all.
    #[error("verdict is not a JSON object: {0}")]
    InvalidJson(String),
    /// The action field is missing or not a string.
    #[error("verdict action is missing or not a string")]
    MissingAction,
    /// The action is not one of the documented values.
    #[error("unknown observer action: {0}")]
    UnknownAction(String),
    /// A required field is absent or empty.
    #[error("verdict field is missing or empty: {0}")]
    MissingField(&'static str),
    /// `reason` exceeds 20 words.
    #[error("verdict reason has {0} words (max 20)")]
    ReasonTooManyWords(usize),
    /// `reason` exceeds 160 characters.
    #[error("verdict reason has {0} characters (max 160)")]
    ReasonTooLong(usize),
    /// Nudge text is missing.
    #[error("nudge verdict requires a message")]
    MissingMessage,
    /// Nudge text is too long.
    #[error("nudge message has {0} characters (max {1})")]
    MessageTooLong(usize, usize),
    /// Repair action without a repair object.
    #[error("repair verdict requires a repair object")]
    RepairRequired,
    /// Repair object with a non-repair action.
    #[error("repair object is only valid with action=repair")]
    RepairNotAllowed,
    /// Repair object without a kind.
    #[error("repair object requires a kind")]
    MissingRepairKind,
    /// Unknown repair kind.
    #[error("unknown repair kind: {0}")]
    UnknownRepairKind(String),
    /// `goal_done` requires evidence ids.
    #[error("goal_done requires at least one evidence id")]
    MissingEvidence,
}

/// Parse and validate a model verdict.
///
/// Accepts bare JSON or a single ```json / ``` fence. Unknown fields are
/// ignored; every documented rule is enforced.
pub fn parse_verdict(text: &str, cfg: &ObserverConfig) -> Result<ObserverVerdict, VerdictError> {
    let body = unwrap_fence(text);
    let value: Value =
        serde_json::from_str(body).map_err(|error| VerdictError::InvalidJson(error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| VerdictError::InvalidJson("expected an object".to_owned()))?;

    let action_value = object.get("action").ok_or(VerdictError::MissingAction)?;
    let action_name = action_value.as_str().ok_or(VerdictError::MissingAction)?;
    let action = parse_action(action_name)?;

    let reason = optional_string(object.get("reason"))?.unwrap_or_default();
    validate_reason(&reason, action)?;

    let target = optional_string(object.get("target"))?.filter(|value| !value.is_empty());
    if action.requires_target() && target.is_none() {
        return Err(VerdictError::MissingField("target"));
    }

    let mut message = optional_string(object.get("message"))?.filter(|value| !value.is_empty());
    if action == ObserverAction::Nudge {
        let text = message.as_deref().ok_or(VerdictError::MissingMessage)?;
        // A nudge a few characters over the cap is still a usable nudge.
        // Rejecting the whole verdict turned a slightly long message into an
        // `observer.unavailable` outage, so the text is cut on a character
        // boundary instead, the same way strip `why`/`outcome` are capped.
        if text.chars().count() > cfg.nudge_max_chars {
            message = Some(truncate_chars(text, cfg.nudge_max_chars));
        }
    }

    let repair = match object.get("repair") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_repair(value)?),
    };
    match (action, repair.as_ref()) {
        (ObserverAction::Repair, None) => return Err(VerdictError::RepairRequired),
        (ObserverAction::Repair, Some(_)) | (_, None) => {}
        (_, Some(_)) => return Err(VerdictError::RepairNotAllowed),
    }

    let goal_evidence = match object.get("goal_evidence") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => string_array(value, "goal_evidence")?,
    };
    if action == ObserverAction::GoalDone && goal_evidence.is_empty() {
        return Err(VerdictError::MissingEvidence);
    }

    Ok(ObserverVerdict {
        action,
        target,
        reason,
        message,
        repair,
        goal_evidence,
    })
}

/// Remove one surrounding Markdown code fence, if present.
pub(crate) fn unwrap_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find("```") {
        let after_start = &trimmed[start + 3..];
        let after_tag = after_start.strip_prefix("json").unwrap_or(after_start);
        let after_tag = after_tag.trim_start_matches(['\r', '\n', ' ']);
        if let Some(end) = after_tag.rfind("```") {
            return after_tag[..end].trim();
        }
    }
    trimmed
}

/// Cut `text` to at most `max` characters, ending in `…` when shortened.
fn truncate_chars(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out.push('…');
    out
}

fn parse_action(name: &str) -> Result<ObserverAction, VerdictError> {
    match name {
        "none" => Ok(ObserverAction::None),
        "nudge" => Ok(ObserverAction::Nudge),
        "inspect" => Ok(ObserverAction::Inspect),
        "repair" => Ok(ObserverAction::Repair),
        "goal_done" => Ok(ObserverAction::GoalDone),
        "goal_blocked" => Ok(ObserverAction::GoalBlocked),
        "goal_failed" => Ok(ObserverAction::GoalFailed),
        other => Err(VerdictError::UnknownAction(other.to_owned())),
    }
}

fn parse_repair(value: &Value) -> Result<Repair, VerdictError> {
    let object = value
        .as_object()
        .ok_or_else(|| VerdictError::InvalidJson("repair must be an object".to_owned()))?;
    let kind_value = object.get("kind").ok_or(VerdictError::MissingRepairKind)?;
    let kind_name = kind_value.as_str().ok_or(VerdictError::MissingRepairKind)?;
    let kind = match kind_name {
        "retry_tool" => RepairKind::RetryTool,
        "cancel_tool" => RepairKind::CancelTool,
        "restart_subagent" => RepairKind::RestartSubagent,
        "compact_context" => RepairKind::CompactContext,
        "switch_model" => RepairKind::SwitchModel,
        "repost" => RepairKind::Repost,
        other => return Err(VerdictError::UnknownRepairKind(other.to_owned())),
    };
    let args = object
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    if !args.is_object() {
        return Err(VerdictError::InvalidJson(
            "repair args must be an object".to_owned(),
        ));
    }
    Ok(Repair {
        kind: Some(kind),
        args,
    })
}

fn validate_reason(reason: &str, action: ObserverAction) -> Result<(), VerdictError> {
    if action != ObserverAction::None && reason.trim().is_empty() {
        return Err(VerdictError::MissingField("reason"));
    }
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(VerdictError::ReasonTooLong(reason.chars().count()));
    }
    let words = reason.split_whitespace().count();
    if words > MAX_REASON_WORDS {
        return Err(VerdictError::ReasonTooManyWords(words));
    }
    Ok(())
}

fn optional_string(value: Option<&Value>) -> Result<Option<String>, VerdictError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(VerdictError::InvalidJson(
            "expected a string field".to_owned(),
        )),
    }
}

fn string_array(value: &Value, field: &'static str) -> Result<Vec<String>, VerdictError> {
    let array = value.as_array().ok_or(VerdictError::InvalidJson(format!(
        "{field} must be an array"
    )))?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let text = item
            .as_str()
            .ok_or_else(|| VerdictError::InvalidJson(format!("{field} must contain strings")))?;
        if !text.is_empty() {
            out.push(text.to_owned());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> ObserverConfig {
        ObserverConfig {
            nudge_max_chars: 40,
            ..ObserverConfig::default()
        }
    }

    #[test]
    fn parses_valid_verdicts_and_fences() {
        let none = parse_verdict(r#"{"action":"none","reason":"on schedule"}"#, &cfg())
            .expect("none verdict");
        assert_eq!(none.action, ObserverAction::None);

        let nudge = parse_verdict(
            "```json\n{\"action\":\"nudge\",\"target\":\"ses_child\",\
             \"reason\":\"stalled with no activity\",\"message\":\"check the failing test\"}\n```",
            &cfg(),
        )
        .expect("fenced nudge");
        assert_eq!(nudge.action, ObserverAction::Nudge);
        assert_eq!(nudge.target.as_deref(), Some("ses_child"));
        assert_eq!(nudge.message.as_deref(), Some("check the failing test"));

        let repair = parse_verdict(
            r#"{"action":"repair","target":"tc_1","reason":"hung shell",
                "repair":{"kind":"cancel_tool","args":{}}}"#,
            &cfg(),
        )
        .expect("repair");
        assert_eq!(
            repair.repair.as_ref().and_then(|repair| repair.kind),
            Some(RepairKind::CancelTool)
        );

        let done = parse_verdict(
            r#"{"action":"goal_done","target":"gol_1","reason":"all criteria verified",
                "goal_evidence":["tc_1","msg_2"]}"#,
            &cfg(),
        )
        .expect("done");
        assert_eq!(done.goal_evidence.len(), 2);

        let with_preamble = parse_verdict(
            "Here is the supervisor evaluation:\n```json\n{\"action\":\"none\",\"reason\":\"on track\"}\n```\nHope this helps.",
            &cfg(),
        )
        .expect("fence with preamble");
        assert_eq!(with_preamble.action, ObserverAction::None);
    }

    #[test]
    fn rejects_invalid_verdicts() {
        assert!(matches!(
            parse_verdict(r#"{"action":"launch_missiles"}"#, &cfg()),
            Err(VerdictError::UnknownAction(_))
        ));
        assert!(matches!(
            parse_verdict(
                r#"{"action":"repair","target":"tc_1","reason":"x"}"#,
                &cfg()
            ),
            Err(VerdictError::RepairRequired)
        ));
        assert!(matches!(
            parse_verdict(
                r#"{"action":"goal_done","target":"gol_1","reason":"done","goal_evidence":[]}"#,
                &cfg()
            ),
            Err(VerdictError::MissingEvidence)
        ));
        assert!(matches!(
            parse_verdict(r#"{"action":"nudge","reason":"needs a nudge"}"#, &cfg()),
            Err(VerdictError::MissingField("target"))
        ));
        let long = parse_verdict(
            r#"{"action":"nudge","target":"ses_1","reason":"slow",
                "message":"012345678901234567890123456789012345678901"}"#,
            &cfg(),
        )
        .expect("an over-long nudge is truncated, not rejected");
        let text = long.message.expect("nudge message");
        assert_eq!(text.chars().count(), 40);
        assert!(text.ends_with('…'));
        assert!(text.starts_with("0123456789"));
        let long_reason = "word ".repeat(21);
        assert!(matches!(
            parse_verdict(
                &json!({"action": "nudge", "target": "ses_1", "message": "go",
                        "reason": long_reason})
                .to_string(),
                &cfg()
            ),
            Err(VerdictError::ReasonTooManyWords(21))
        ));
        assert!(matches!(
            parse_verdict(r#"{"action":"none","repair":{"kind":"repost"}}"#, &cfg()),
            Err(VerdictError::RepairNotAllowed)
        ));
        assert!(matches!(
            parse_verdict("not json", &cfg()),
            Err(VerdictError::InvalidJson(_))
        ));
    }
}
