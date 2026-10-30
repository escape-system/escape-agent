use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use oc_config::SkillSource;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::activate::find_skill;
use super::util::{optional_bool, publish, required_str};
use super::{MAX_DESCRIPTION_CHARS, MAX_NAME_CHARS};

pub(super) struct SkillWriteTool {
    pub(super) runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for SkillWriteTool {
    fn name(&self) -> &'static str {
        "skill_write"
    }

    fn description(&self) -> &'static str {
        "Author or update a skill as SKILL.md. Project scope writes into .opencode, global scope into the config directory."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[a-z0-9][a-z0-9-]{1,63}$",
                    "description": "Kebab-case skill id (2-64 characters)."
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable skill name."
                },
                "description": {
                    "type": "string",
                    "description": "When to use the skill (at most 200 characters)."
                },
                "body": {
                    "type": "string",
                    "description": "Skill instructions in Markdown; kept verbatim."
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global"],
                    "description": "Where to write the skill."
                },
                "autoinvoke": {
                    "type": "boolean",
                    "description": "Advertise the skill in the prompt index (default true)."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing skill that was not authored by the agent."
                }
            },
            "required": ["id", "name", "description", "body", "scope"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let id = required_str(&input, "id")?;
        validate_id(&id).map_err(ToolError::new)?;
        let name = required_str(&input, "name")?;
        validate_name(&name).map_err(ToolError::new)?;
        let description = required_str(&input, "description")?;
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(ToolError::new(format!(
                "description must be at most {MAX_DESCRIPTION_CHARS} characters"
            )));
        }
        if description.chars().any(char::is_control) {
            return Err(ToolError::new(
                "description must be a single line without control characters",
            ));
        }
        let body = input
            .get("body")
            .and_then(Value::as_str)
            .filter(|body| !body.trim().is_empty())
            .ok_or_else(|| ToolError::new("`body` is required and must not be empty"))?;
        let scope = required_str(&input, "scope")?;
        if scope != "project" && scope != "global" {
            return Err(ToolError::new("`scope` must be `project` or `global`"));
        }
        let autoinvoke = optional_bool(&input, "autoinvoke")?;
        let overwrite = optional_bool(&input, "overwrite")?.unwrap_or(false);

        let config = &self.runtime.inner.config.config;
        let target = match scope.as_str() {
            "project" => ctx
                .directory
                .join(".opencode")
                .join("skill")
                .join(&id)
                .join("SKILL.md"),
            _ => config
                .global_config_dir()
                .ok_or_else(|| ToolError::new("global configuration directory is unavailable"))?
                .join("skill")
                .join(&id)
                .join("SKILL.md"),
        };

        let existing = find_skill(config, &id);
        let created = existing.is_none() && !target.exists();
        if !overwrite {
            if let Some(existing) = &existing
                && existing.source != SkillSource::Agent
            {
                return Err(ToolError::new(format!(
                    "skill `{id}` already exists at {} from source {:?}; pass `overwrite: true` to replace it",
                    existing.path.display(),
                    existing.source
                )));
            }
            if target.exists() {
                return Err(ToolError::new(format!(
                    "{} already exists; pass `overwrite: true` to replace it",
                    target.display()
                )));
            }
        }

        let document = compose_document(&name, &description, autoinvoke, body);
        write_atomic(&target, &document)?;
        publish(
            &self.runtime,
            ctx,
            "skill.updated",
            json!({
                "id": id,
                "name": name,
                "path": target.to_string_lossy(),
                "source": "agent",
            }),
        );

        Ok(ToolResult::text(
            serde_json::to_string_pretty(&json!({
                "id": id,
                "name": name,
                "description": description,
                "scope": scope,
                "path": target.to_string_lossy(),
                "source": "agent",
                "created": created,
            }))
            .unwrap_or_else(|_| target.to_string_lossy().into_owned()),
        ))
    }
}

pub(super) fn validate_id(id: &str) -> Result<(), String> {
    let mut chars = id.chars();
    let first = chars
        .next()
        .ok_or_else(|| "skill id must not be empty".to_owned())?;
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "invalid skill id `{id}`: must match ^[a-z0-9][a-z0-9-]{{1,63}}$"
        ));
    }
    let rest: Vec<char> = chars.collect();
    if rest.is_empty() || rest.len() > 63 {
        return Err(format!(
            "invalid skill id `{id}`: must match ^[a-z0-9][a-z0-9-]{{1,63}}$"
        ));
    }
    if let Some(bad) = rest
        .iter()
        .find(|ch| !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && **ch != '-')
    {
        return Err(format!(
            "invalid skill id `{id}`: `{bad}` is not allowed; use lowercase letters, digits and `-`"
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("name must be at most {MAX_NAME_CHARS} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("name must be a single line without control characters".to_owned());
    }
    Ok(())
}

pub(super) fn compose_document(
    name: &str,
    description: &str,
    autoinvoke: Option<bool>,
    body: &str,
) -> String {
    let mut out = String::with_capacity(body.len() + 160);
    out.push_str("---\n");
    let _ = writeln!(out, "name: {}", yaml_string(name));
    let _ = writeln!(out, "description: {}", yaml_string(description));
    out.push_str("metadata:\n  opencode/agent: true\n");
    if let Some(autoinvoke) = autoinvoke {
        let _ = writeln!(out, "  opencode/autoinvoke: {autoinvoke}");
    }
    out.push_str("---\n");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{}\"", value.replace('"', "'")))
}

fn write_atomic(path: &Path, text: &str) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::new(format!("{} has no parent directory", path.display())))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| ToolError::new(format!("cannot create {}: {error}", parent.display())))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = parent.join(format!(".SKILL.md.tmp-{}-{stamp}", std::process::id()));
    if let Err(error) = std::fs::write(&temp, text) {
        let _ = std::fs::remove_file(&temp);
        return Err(ToolError::new(format!(
            "cannot write {}: {error}",
            temp.display()
        )));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(ToolError::new(format!(
            "cannot replace {}: {error}",
            path.display()
        )));
    }
    Ok(())
}
