//! The mandatory tool-call expectation contract.
//!
//! Every tool schema gets an `expectation` property before the provider
//! request; the dispatcher strips it before executing the tool. The observer
//! receives only these fields (never arguments or output).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Maximum `why` length in characters.
pub const MAX_WHY: usize = 160;
/// Upper bound for both clocks.
pub const MAX_SECONDS: u32 = 3600;
/// The observer must not intervene before the agent's own expectation plus a
/// two-second grace.
pub const GRACE_SECONDS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expectation {
    /// One sentence: action + expected outcome. Never contains secrets.
    pub why: String,
    /// Agent's own estimate of how long the call should take.
    pub expect_seconds: u32,
    /// Observer deadline; must be `expect_seconds + 2` or later.
    pub intervene_after_seconds: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpectationError {
    #[error("expectation.why is empty or longer than {MAX_WHY} characters")]
    Why,
    #[error("expectation.expect_seconds must be between 1 and {MAX_SECONDS}")]
    Expect,
    #[error("expectation must be an object with why/expect_seconds/intervene_after_seconds")]
    Shape,
}

impl Expectation {
    /// Validate and normalize. Correctable problems (too-short intervene
    /// window, out-of-range clocks) are fixed rather than rejected; only a
    /// missing/empty/oversized `why` and a non-object shape are errors.
    pub fn normalize(mut self) -> Result<Self, ExpectationError> {
        if self.why.trim().is_empty() || self.why.chars().count() > MAX_WHY {
            return Err(ExpectationError::Why);
        }
        if self.expect_seconds == 0 {
            return Err(ExpectationError::Expect);
        }
        self.expect_seconds = self.expect_seconds.min(MAX_SECONDS);
        self.intervene_after_seconds = self
            .intervene_after_seconds
            .max(self.expect_seconds.saturating_add(GRACE_SECONDS))
            .min(MAX_SECONDS);
        Ok(self)
    }

    /// Defaults by tool class when a provider omits the field entirely.
    #[must_use]
    pub fn derived(tool: &str) -> Self {
        let (expect, why) = match tool {
            "read" | "glob" | "grep" | "lsp" | "lsp_diagnostics" | "code_map" | "roster"
            | "digest" => (10, "inspect the workspace and return a compact result"),
            "skill" | "skill_lint" => (10, "inspect a skill and return its content"),
            "write" | "edit" | "patch" | "symbol_edit" | "memory_write" => {
                (20, "apply a deterministic file or memory change")
            }
            "skill_write" => (30, "persist a skill change on disk"),
            "goal_set" | "goal_update" | "goal_complete" | "goal_status" => {
                (30, "read or update the session goal state")
            }
            "lane" => (600, "queue or inspect a build-lane ticket"),
            "shell" => (300, "run a command and return its result"),
            "subagent" | "talk" | "session_write" => (600, "delegate work and wait for a result"),
            "webfetch" | "websearch" => (30, "fetch external information"),
            _ => (60, "run the tool and return a result"),
        };
        Self {
            why: why.to_owned(),
            expect_seconds: expect,
            intervene_after_seconds: expect + GRACE_SECONDS,
        }
    }
}

/// Add the `expectation` property and mark it required.
///
/// The input schema is expected to be an object schema (as all function
/// schemas are). Existing `properties` are preserved; `additionalProperties`
/// is left untouched (many MCP schemas set it to false and still accept the
/// extra key because we strip it before dispatch).
pub fn inject(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let properties = object
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert(
            "expectation".to_owned(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["why", "expect_seconds", "intervene_after_seconds"],
                "properties": {
                    "why": {
                        "type": "string",
                        "maxLength": MAX_WHY,
                        "description": "One sentence: the action and the expected outcome."
                    },
                    "expect_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_SECONDS,
                        "description": "How long you expect this call to take."
                    },
                    "intervene_after_seconds": {
                        "type": "integer",
                        "minimum": 3,
                        "maximum": MAX_SECONDS,
                        "description": "When the observer may intervene; must exceed expect_seconds."
                    }
                }
            }),
        );
    }
    let required = object
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(required) = required.as_array_mut()
        && !required.iter().any(|value| value == "expectation")
    {
        required.push(Value::String("expectation".to_owned()));
    }
}

fn parse_u32(value: &Value) -> Option<u32> {
    if let Some(n) = value.as_u64() {
        return u32::try_from(n).ok();
    }
    if let Some(s) = value.as_str() {
        return s.trim().parse::<u32>().ok();
    }
    None
}

/// Remove and validate the expectation from a tool input.
///
/// Returns the derived expectation when the model omitted the field, or parses
/// leniently if the model sent string numbers or root-level expectation clocks.
pub fn extract(input: &mut Value, tool: &str) -> Result<Expectation, ExpectationError> {
    let Some(object) = input.as_object_mut() else {
        return Ok(Expectation::derived(tool));
    };

    // Clean up any stray top-level clocks models sometimes place at root.
    let top_expect = object.remove("expect_seconds").and_then(|v| parse_u32(&v));
    let top_intervene = object
        .remove("intervene_after_seconds")
        .and_then(|v| parse_u32(&v));

    let derived = Expectation::derived(tool);

    let Some(raw) = object.remove("expectation") else {
        if top_expect.is_some() || top_intervene.is_some() {
            let mut exp = derived;
            if let Some(e) = top_expect {
                exp.expect_seconds = e;
            }
            if let Some(i) = top_intervene {
                exp.intervene_after_seconds = i;
            }
            return exp.normalize();
        }
        return Ok(derived);
    };

    if let Some(why_str) = raw.as_str() {
        let mut exp = derived;
        let trimmed = why_str.trim();
        if !trimmed.is_empty() {
            exp.why = trimmed.chars().take(MAX_WHY).collect();
        }
        if let Some(e) = top_expect {
            exp.expect_seconds = e;
        }
        if let Some(i) = top_intervene {
            exp.intervene_after_seconds = i;
        }
        return exp.normalize();
    }

    if raw.is_null() || raw.is_boolean() {
        let mut exp = derived;
        if let Some(e) = top_expect {
            exp.expect_seconds = e;
        }
        if let Some(i) = top_intervene {
            exp.intervene_after_seconds = i;
        }
        return exp.normalize();
    }

    if let Some(map) = raw.as_object() {
        let mut exp = derived;
        if let Some(why_val) = map.get("why").and_then(Value::as_str) {
            if why_val.trim().is_empty() {
                return Err(ExpectationError::Why);
            }
            exp.why = why_val.trim().chars().take(MAX_WHY).collect();
        }
        if let Some(e) = map.get("expect_seconds").and_then(parse_u32).or(top_expect) {
            exp.expect_seconds = e;
        }
        if let Some(i) = map
            .get("intervene_after_seconds")
            .and_then(parse_u32)
            .or(top_intervene)
        {
            exp.intervene_after_seconds = i;
        }
        return exp.normalize();
    }

    match serde_json::from_value::<Expectation>(raw) {
        Ok(exp) => exp.normalize(),
        Err(_) => Ok(derived),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_required_property_and_preserves_existing() {
        let mut schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        });
        inject(&mut schema);
        assert_eq!(schema["required"], json!(["path", "expectation"]));
        assert!(schema["properties"]["path"].is_object());
        assert_eq!(
            schema["properties"]["expectation"]["required"],
            json!(["why", "expect_seconds", "intervene_after_seconds"])
        );
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn injection_is_idempotent() {
        let mut schema = json!({"type": "object"});
        inject(&mut schema);
        inject(&mut schema);
        assert_eq!(
            schema["required"].as_array().unwrap().len(),
            1,
            "expectation must appear once"
        );
    }

    #[test]
    fn extract_strips_and_normalizes() {
        let mut input = json!({
            "path": "a.rs",
            "expectation": {"why": "read a file", "expect_seconds": 5, "intervene_after_seconds": 6}
        });
        let expectation = extract(&mut input, "read").unwrap();
        assert_eq!(expectation.expect_seconds, 5);
        assert_eq!(expectation.intervene_after_seconds, 7, "grace is enforced");
        assert!(input.get("expectation").is_none());
        assert_eq!(input["path"], "a.rs");
    }

    #[test]
    fn missing_expectation_uses_tool_class_default() {
        let mut input = json!({"command": "cargo test"});
        let expectation = extract(&mut input, "shell").unwrap();
        assert_eq!(expectation.expect_seconds, 300);
        assert!(expectation.intervene_after_seconds > 300);
    }

    #[test]
    fn invalid_why_is_rejected() {
        let mut input = json!({
            "expectation": {"why": "  ", "expect_seconds": 1, "intervene_after_seconds": 3}
        });
        assert_eq!(extract(&mut input, "read"), Err(ExpectationError::Why));
    }

    #[test]
    fn clocks_are_clamped() {
        let mut input = json!({
            "expectation": {"why": "wait", "expect_seconds": 99_999, "intervene_after_seconds": 99_999}
        });
        let expectation = extract(&mut input, "shell").unwrap();
        assert_eq!(expectation.expect_seconds, MAX_SECONDS);
        assert_eq!(expectation.intervene_after_seconds, MAX_SECONDS);
    }

    #[test]
    fn derived_table_matches_the_documented_classes() {
        assert_eq!(Expectation::derived("read").expect_seconds, 10);
        assert_eq!(Expectation::derived("edit").expect_seconds, 20);
        assert_eq!(Expectation::derived("shell").expect_seconds, 300);
        assert_eq!(Expectation::derived("subagent").expect_seconds, 600);
        assert_eq!(Expectation::derived("mcp_github_search").expect_seconds, 60);
        assert_eq!(Expectation::derived("goal_set").expect_seconds, 30);
        assert_eq!(Expectation::derived("goal_complete").expect_seconds, 30);
        assert_eq!(Expectation::derived("skill").expect_seconds, 10);
        assert_eq!(Expectation::derived("skill_lint").expect_seconds, 10);
        assert_eq!(Expectation::derived("skill_write").expect_seconds, 30);
        assert_eq!(Expectation::derived("lane").expect_seconds, 600);
    }

    #[test]
    fn extract_leniently_parses_strings_and_top_level_clocks() {
        // String numbers in expectation object
        let mut input1 = json!({
            "command": "uptime",
            "expectation": {
                "why": "check load",
                "expect_seconds": "2",
                "intervene_after_seconds": "10"
            }
        });
        let exp1 = extract(&mut input1, "shell").unwrap();
        assert_eq!(exp1.expect_seconds, 2);
        assert_eq!(exp1.intervene_after_seconds, 10);
        assert_eq!(exp1.why, "check load");

        // Top-level clocks and null expectation
        let mut input2 = json!({
            "command": "cat /proc/loadavg",
            "expect_seconds": "2",
            "intervene_after_seconds": "10",
            "expectation": null
        });
        let exp2 = extract(&mut input2, "shell").unwrap();
        assert_eq!(exp2.expect_seconds, 2);
        assert_eq!(exp2.intervene_after_seconds, 10);
        assert_eq!(exp2.why, "run a command and return its result");
        assert!(input2.get("expect_seconds").is_none());
        assert!(input2.get("intervene_after_seconds").is_none());
        assert!(input2.get("expectation").is_none());
        assert_eq!(input2["command"], "cat /proc/loadavg");

        // String expectation
        let mut input3 = json!({
            "path": "test.txt",
            "expectation": "read the config file"
        });
        let exp3 = extract(&mut input3, "read").unwrap();
        assert_eq!(exp3.why, "read the config file");
        assert_eq!(exp3.expect_seconds, 10);
    }
}
