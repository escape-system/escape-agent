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
fn lint(config: &ConfigService, path: &Path, id: Option<&str>, text: &str) -> Vec<Value> {
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
    if parsed.body.trim().is_empty() {
        findings.push(finding("warning", "empty_body", "the skill body is empty"));
    }
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
