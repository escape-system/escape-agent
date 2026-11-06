//! The asynchronous memory scribe.
//!
//! The host owns the model: it implements [`ScribeModel`] over `oc-llm` and
//! calls [`run_scribe`] after a root turn settles. This crate supplies the
//! prompt, a strict parser for the model's JSON array and the filter that keeps
//! only durable, non-secret candidates.

use std::collections::HashSet;
use std::pin::Pin;

use serde_json::{Map, Value};

use crate::error::MemoryError;
use crate::secret::detect_secret;
use crate::service::{DEFAULT_CONFIDENCE, DEFAULT_IMPORTANCE, TEXT_CAP};

/// Boxed future returned by [`ScribeModel::complete`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A one-shot text completion. The host adapts its model client to this.
pub trait ScribeModel: Send + Sync {
    /// Completes `prompt` and returns the model's text.
    fn complete<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Result<String, MemoryError>>;
}

/// One memory proposed by the scribe model.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCandidate {
    /// Memory text.
    pub text: String,
    /// `fact`, `decision`, `preference`, `pattern` or `gotcha`.
    pub kind: String,
    /// Short tags.
    pub tags: Vec<String>,
    /// 1..=5.
    pub importance: u8,
    /// 0.0..=1.0.
    pub confidence: f32,
}

/// Maximum candidates kept from one scribe run.
pub const MAX_CANDIDATES: usize = 5;
/// Minimum confidence a candidate needs to be stored.
pub const MIN_CONFIDENCE: f32 = 0.6;
/// Minimum accepted text length.
pub const MIN_TEXT_CHARS: usize = 20;
/// Maximum tags kept per candidate.
pub const MAX_TAGS: usize = 8;

const KINDS: [&str; 5] = ["fact", "decision", "preference", "pattern", "gotcha"];
const KIND_LIST: &str = "fact|decision|preference|pattern|gotcha";
const TRANSIENT_PHRASES: [&str; 3] = ["currently", "running", "just now"];

/// Builds the scribe prompt around a session digest.
#[must_use]
pub fn scribe_prompt(recent: &str) -> String {
    format!(
        "You extract durable memories from one coding session.\n\
         \n\
         Return a JSON array only — no prose and no code fence — with at most \
         {MAX_CANDIDATES} objects. Each object has exactly these fields:\n\
         - \"text\": one self-contained statement, {MIN_TEXT_CHARS}-{TEXT_CAP} characters, \
           no credentials\n\
         - \"kind\": one of {KIND_LIST}\n\
         - \"tags\": array of short lowercase strings, may be empty\n\
         - \"importance\": integer 1-5\n\
         - \"confidence\": number 0-1\n\
         \n\
         Record only durable facts, decisions, preferences, patterns and gotchas that a \
         future session cannot derive from the repository. Never record transient state \
         (currently, running, just now), secrets, or task progress. Return [] when nothing \
         is durable.\n\
         \n\
         Session digest:\n\
         {recent}\n"
    )
}

/// Parses the model's JSON array into candidates.
///
/// The top level must be an array; elements that are not objects, lack a
/// non-empty `text`, or name an unknown `kind` are dropped. Importance and
/// confidence are clamped, missing values use the defaults, and duplicate texts
/// (normalized) are collapsed. At most [`MAX_CANDIDATES`] survive.
pub fn parse_candidates(json: &str) -> Result<Vec<MemoryCandidate>, MemoryError> {
    let cleaned = strip_code_fence(json);
    let value: Value =
        serde_json::from_str(cleaned).map_err(|error| MemoryError::ModelJson(error.to_string()))?;
    let items = value
        .as_array()
        .ok_or_else(|| MemoryError::ModelJson("expected a top-level JSON array".to_owned()))?;

    let mut candidates = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for item in items {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(text) = object.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = normalize(text);
        if text.is_empty() {
            continue;
        }
        let Some(kind) = object.get("kind").and_then(Value::as_str) else {
            continue;
        };
        let kind = kind.trim().to_lowercase();
        if !KINDS.contains(&kind.as_str()) {
            continue;
        }
        let identity = text.to_lowercase();
        if !seen.insert(identity) {
            continue;
        }
        candidates.push(MemoryCandidate {
            text,
            kind,
            tags: parse_tags(object),
            importance: parse_importance(object.get("importance")),
            confidence: parse_confidence(object.get("confidence")),
        });
    }
    Ok(candidates)
}

/// Keeps candidates that pass every scribe filter, in order, at most
/// [`MAX_CANDIDATES`].
#[must_use]
pub fn filter_candidates(
    candidates: impl IntoIterator<Item = MemoryCandidate>,
) -> Vec<MemoryCandidate> {
    let mut kept: Vec<MemoryCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for mut candidate in candidates {
        if kept.len() >= MAX_CANDIDATES {
            break;
        }
        let text = normalize(&candidate.text);
        let chars = text.chars().count();
        if !(MIN_TEXT_CHARS..=TEXT_CAP).contains(&chars) {
            continue;
        }
        if !candidate.confidence.is_finite() || candidate.confidence < MIN_CONFIDENCE {
            continue;
        }
        if detect_secret(&text).is_some() {
            continue;
        }
        let lower = text.to_lowercase();
        if TRANSIENT_PHRASES
            .iter()
            .any(|phrase| lower.contains(phrase))
        {
            continue;
        }
        if !seen.insert(lower) {
            continue;
        }
        candidate.text = text;
        candidate.importance = candidate.importance.clamp(1, 5);
        kept.push(candidate);
    }
    kept
}

/// Prompts the model, parses its array and filters the candidates.
pub async fn run_scribe(
    model: &dyn ScribeModel,
    input: &str,
) -> Result<Vec<MemoryCandidate>, MemoryError> {
    let raw = model.complete(&scribe_prompt(input)).await?;
    Ok(filter_candidates(parse_candidates(&raw)?))
}

fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let after_header = rest.find('\n').map_or(rest, |newline| &rest[newline + 1..]);
        if let Some(body) = after_header.trim_end().strip_suffix("```") {
            return body.trim();
        }
    }
    trimmed
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_tags(object: &Map<String, Value>) -> Vec<String> {
    let mut tags = Vec::new();
    if let Some(items) = object.get("tags").and_then(Value::as_array) {
        for item in items {
            let Some(tag) = item.as_str() else {
                continue;
            };
            let tag = tag.trim();
            if tag.is_empty() || tags.iter().any(|existing: &String| existing == tag) {
                continue;
            }
            tags.push(tag.to_owned());
            if tags.len() >= MAX_TAGS {
                break;
            }
        }
    }
    tags
}

fn parse_importance(value: Option<&Value>) -> u8 {
    value
        .and_then(Value::as_i64)
        .map_or(DEFAULT_IMPORTANCE, |importance| {
            u8::try_from(importance.clamp(1, 5)).unwrap_or(DEFAULT_IMPORTANCE)
        })
}

#[allow(clippy::cast_possible_truncation)] // clamped to 0.0..=1.0 first
fn parse_confidence(value: Option<&Value>) -> f32 {
    value
        .and_then(Value::as_f64)
        .map_or(DEFAULT_CONFIDENCE, |confidence| {
            confidence.clamp(0.0, 1.0) as f32
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(text: &str, confidence: f32) -> MemoryCandidate {
        MemoryCandidate {
            text: text.to_owned(),
            kind: "fact".to_owned(),
            tags: Vec::new(),
            importance: 3,
            confidence,
        }
    }

    struct Fake {
        response: String,
    }

    impl ScribeModel for Fake {
        fn complete<'a>(&'a self, prompt: &'a str) -> BoxFuture<'a, Result<String, MemoryError>> {
            let response = self.response.clone();
            let prompt_ok = prompt.contains("JSON array") && prompt.contains("Session digest:");
            Box::pin(async move {
                if prompt_ok {
                    Ok(response)
                } else {
                    Err(MemoryError::Model("prompt missing sections".to_owned()))
                }
            })
        }
    }

    struct Failing;

    impl ScribeModel for Failing {
        fn complete<'a>(&'a self, _prompt: &'a str) -> BoxFuture<'a, Result<String, MemoryError>> {
            Box::pin(async move { Err(MemoryError::Model("provider down".to_owned())) })
        }
    }

    #[test]
    fn prompt_states_the_contract() {
        let prompt = scribe_prompt("user asked for X\nassistant did Y");
        assert!(prompt.contains("JSON array"), "{prompt}");
        assert!(prompt.contains("at most 5"), "{prompt}");
        assert!(prompt.contains("fact|decision|preference|pattern|gotcha"));
        assert!(prompt.contains(&format!("{MIN_TEXT_CHARS}-{TEXT_CAP} characters")));
        assert!(prompt.contains("Session digest:\nuser asked for X"));
        assert!(prompt.contains("Return [] when nothing is durable"));
    }

    #[test]
    fn parser_accepts_clamps_and_drops() {
        let json = r#"[
            {"text": "  the project uses a single sqlite writer for state  ",
             "kind": "Decision", "tags": ["db", "db", " ", 7],
             "importance": 9, "confidence": 2.0},
            {"text": "unknown kinds are dropped entirely", "kind": "hunch"},
            {"text": "", "kind": "fact"},
            {"kind": "fact"},
            {"text": "negative values clamp to the minimum allowed here",
             "kind": "gotcha", "importance": -3, "confidence": -1}
        ]"#;
        let candidates = parse_candidates(json).expect("parse");
        assert_eq!(candidates.len(), 2, "{candidates:?}");
        assert_eq!(
            candidates[0].text,
            "the project uses a single sqlite writer for state"
        );
        assert_eq!(candidates[0].kind, "decision");
        assert_eq!(candidates[0].tags, vec!["db"]);
        assert_eq!(candidates[0].importance, 5);
        assert!((candidates[0].confidence - 1.0).abs() < f32::EPSILON);
        assert_eq!(candidates[1].importance, 1);
        assert!((candidates[1].confidence - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parser_defaults_and_deduplicates() {
        let json = r#"[
            {"text": "durable fact about the build pipeline", "kind": "fact"},
            {"text": "Durable   fact about the build pipeline", "kind": "fact"}
        ]"#;
        let candidates = parse_candidates(json).expect("parse");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].importance, DEFAULT_IMPORTANCE);
        assert!((candidates[0].confidence - DEFAULT_CONFIDENCE).abs() < f32::EPSILON);
    }

    #[test]
    fn parser_is_strict_about_the_top_level() {
        let error = parse_candidates(r#"{"text": "object"}"#).unwrap_err();
        assert!(matches!(error, MemoryError::ModelJson(_)), "{error:?}");
        let error = parse_candidates("not json at all").unwrap_err();
        assert!(matches!(error, MemoryError::ModelJson(_)), "{error:?}");
    }

    #[test]
    fn parser_strips_markdown_code_fences() {
        let fenced = "```json\n[\n  {\"text\": \"fenced durable fact about the sqlite writer\", \"kind\": \"fact\", \"importance\": 4, \"confidence\": 0.85}\n]\n```";
        let candidates = parse_candidates(fenced).expect("fenced json array");
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].text,
            "fenced durable fact about the sqlite writer"
        );
        assert_eq!(candidates[0].importance, 4);
    }

    #[test]
    fn filter_keeps_only_durable_clean_candidates() {
        let long = "the workspace pins sqlite to wal mode with a five second busy timeout";
        let candidates = vec![
            candidate(long, 0.9),
            candidate("confidence below the threshold is dropped here", 0.4),
            candidate("too short", 0.9),
            candidate(
                "deploy with sk-proj-abcdefghijklmnopqrstuvwxyz in the config",
                0.9,
            ),
            candidate("currently the migration is running in the background", 0.9),
            candidate(long, 0.95),
        ];
        let kept = filter_candidates(candidates);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].text, long);
        assert!((kept[0].confidence - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn filter_caps_at_five() {
        let candidates = (0..8).map(|index| {
            candidate(
                &format!("durable fact number {index} about keeping the writer single"),
                0.9,
            )
        });
        assert_eq!(filter_candidates(candidates).len(), MAX_CANDIDATES);
    }

    #[tokio::test]
    async fn run_scribe_parses_and_filters() {
        let model = Fake {
            response: r#"[
                {"text": "tests use an in-memory sqlite store and never hit the network",
                 "kind": "pattern", "tags": ["tests"], "importance": 4, "confidence": 0.9},
                {"text": "currently the build is running and this must not be stored",
                 "kind": "fact", "confidence": 0.95},
                {"text": "low confidence candidate that should be dropped", "kind": "fact",
                 "confidence": 0.2}
            ]"#
            .to_owned(),
        };
        let kept = run_scribe(&model, "digest line").await.expect("scribe");
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].kind, "pattern");
        assert_eq!(kept[0].tags, vec!["tests"]);
    }

    #[tokio::test]
    async fn run_scribe_propagates_model_errors() {
        let error = run_scribe(&Failing, "digest").await.unwrap_err();
        assert!(matches!(error, MemoryError::Model(_)), "{error:?}");
    }
}
