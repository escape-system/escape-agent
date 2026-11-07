use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use oc_config::{ConfigService, parse_markdown};
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::MAX_DESCRIPTION_CHARS;
use super::activate::find_skill;
use super::util::{finding, optional_str};
use super::write::validate_id;

const MAX_BODY_LINES: usize = 200;
const MAX_BODY_CHARS: usize = 12_000;

const BANNED_SHELL_PATTERNS: &[(&str, &str)] = &[
    ("grep ", "the native `grep` tool"),
    ("rg ", "the native `grep` tool"),
    ("cat ", "the native `read` tool"),
    ("head ", "the native `read` tool with `limit`"),
    ("tail ", "the native `read` tool with `offset`"),
    ("sed ", "the native `edit` or `patch` tool"),
    ("awk ", "the native `edit` or `patch` tool"),
    ("find .", "the native `glob` tool"),
];

const HYPE_WORDS: &[&str] = &[
    "cutting-edge",
    "state-of-the-art",
    "revolutionary",
    "seamless",
    "groundbreaking",
];

const FORBIDDEN_FILES: &[&str] = &[".env", ".env.example", ".gitignore"];

pub(super) struct SkillLintTool {
    pub(super) runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for SkillLintTool {
    fn name(&self) -> &'static str {
        "skill_lint"
    }

    fn description(&self) -> &'static str {
        "Validate a skill file without writing: frontmatter, id, description length and duplicate ids."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Skill file or directory, relative to the session directory."
                },
                "id": {
                    "type": "string",
                    "description": "Discovered skill id to lint instead of a path."
                }
            }
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let raw_path = optional_str(&input, "path");
        let id_hint = optional_str(&input, "id");
        let config = &self.runtime.inner.config.config;
        let path = if let Some(raw) = raw_path {
            let path = PathBuf::from(raw);
            let path = if path.is_absolute() {
                path
            } else {
                ctx.directory.join(path)
            };
            if path.is_dir() {
                path.join("SKILL.md")
            } else {
                path
            }
        } else if let Some(id) = id_hint {
            find_skill(config, &ctx.directory, &id)
                .ok_or_else(|| ToolError::new(format!("unknown skill `{id}`")))?
                .path
        } else {
            return Err(ToolError::new("skill_lint needs `path` or `id`"));
        };

        let text = std::fs::read_to_string(&path)
            .map_err(|error| ToolError::new(format!("cannot read {}: {error}", path.display())))?;
        let id = skill_id_from_path(&path);
        let findings = lint(config, &path, id.as_deref(), &text);
        let ok = !findings
            .iter()
            .any(|finding| finding.get("severity").and_then(Value::as_str) == Some("error"));
        Ok(ToolResult::text(
            serde_json::to_string_pretty(&json!({
                "path": path.to_string_lossy(),
                "id": id,
                "ok": ok,
                "findings": findings,
            }))
            .unwrap_or_else(|_| "{\"ok\":false}".to_owned()),
        ))
    }
}

pub(super) fn lint(
    config: &ConfigService,
    path: &Path,
    id: Option<&str>,
    text: &str,
) -> Vec<Value> {
    let mut findings = Vec::new();
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    let has_frontmatter = trimmed
        .lines()
        .next()
        .is_some_and(|line| line.trim() == "---");
    if !has_frontmatter {
        findings.push(finding(
            "warning",
            "missing_frontmatter",
            "the file has no `---` frontmatter block; name defaults to the id",
        ));
    }

    let parsed = parse_markdown(text);
    if let Some(error) = &parsed.error {
        findings.push(finding(
            "error",
            "invalid_frontmatter",
            format!("unparsable frontmatter: {error}"),
        ));
    }
    let name = parsed.meta.get("name").and_then(Value::as_str);
    if name.is_none() {
        findings.push(finding(
            "warning",
            "missing_name",
            "no frontmatter `name`; discovery falls back to the id",
        ));
    }
    match parsed.meta.get("description").and_then(Value::as_str) {
        None => findings.push(finding(
            "warning",
            "missing_description",
            "no description; the skill is not advertised in the prompt index",
        )),
        Some(description) if description.trim().is_empty() => findings.push(finding(
            "warning",
            "empty_description",
            "the description is empty; the skill is not advertised",
        )),
        Some(description) if description.chars().count() > MAX_DESCRIPTION_CHARS => {
            findings.push(finding(
                "error",
                "description_too_long",
                format!("description is longer than {MAX_DESCRIPTION_CHARS} characters"),
            ));
        }
        Some(description) if description.chars().any(char::is_control) => {
            findings.push(finding(
                "warning",
                "description_not_single_line",
                "the description contains a control character (newline or tab)",
            ));
        }
        Some(_) => {}
    }

    lint_body(&parsed.body, &mut findings);
    match id {
        None => findings.push(finding(
            "error",
            "invalid_id",
            "could not derive a skill id from the file name",
        )),
        Some(id) => {
            if let Err(error) = validate_id(id) {
                findings.push(finding("error", "invalid_id", error));
            }
            if config
                .rescan_skills()
                .iter()
                .any(|skill| skill.id == id && skill.path.as_path() != path)
            {
                findings.push(finding(
                    "warning",
                    "duplicate_id",
                    format!("another skill with id `{id}` is already discoverable"),
                ));
            }
        }
    }
    findings
}

fn lint_body(body: &str, findings: &mut Vec<Value>) {
    if body.trim().is_empty() {
        findings.push(finding("warning", "empty_body", "the skill body is empty"));
        return;
    }

    let line_count = body.lines().count();
    let char_count = body.chars().count();
    if line_count > MAX_BODY_LINES || char_count > MAX_BODY_CHARS {
        findings.push(finding(
            "warning",
            "body_too_long",
            format!(
                "body has {line_count} lines and {char_count} characters (soft budget: {MAX_BODY_LINES} lines / {MAX_BODY_CHARS} chars); keep skills focused to avoid context window bloat"
            ),
        ));
    }

    let has_when_to_use = body.lines().any(|line| {
        let line_trimmed = line.trim().trim_start_matches('#').trim();
        line_trimmed.eq_ignore_ascii_case("when to use")
            || line_trimmed.to_lowercase().starts_with("when to use")
    });
    if !has_when_to_use {
        findings.push(finding(
            "warning",
            "missing_when_to_use",
            "body is missing a '## When to use' section; explicit trigger conditions are recommended for reliable agent routing",
        ));
    }

    for (pattern, recommendation) in BANNED_SHELL_PATTERNS {
        if body.contains(pattern) {
            findings.push(finding(
                "warning",
                "banned_shell_utility",
                format!(
                    "skill references raw shell command '{pattern}'; recommend using {recommendation} for structured output and safety"
                ),
            ));
            break;
        }
    }

    let body_lower = body.to_lowercase();
    for word in HYPE_WORDS {
        if body_lower.contains(word) {
            findings.push(finding(
                "warning",
                "marketing_buzzwords",
                format!(
                    "body contains hype adjective '{word}'; prefer concrete technical requirements over promotional language"
                ),
            ));
            break;
        }
    }

    for file in FORBIDDEN_FILES {
        if body.contains(file) {
            findings.push(finding(
                "warning",
                "forbidden_files",
                format!(
                    "body references scaffolding/secret file '{file}'; skills should contain operational workflows, not secret templates or VCS ignore files"
                ),
            ));
            break;
        }
    }
}

pub(super) fn skill_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name == "SKILL.md" {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
            .map(str::to_owned);
    }
    path.file_stem().and_then(OsStr::to_str).map(str::to_owned)
}
