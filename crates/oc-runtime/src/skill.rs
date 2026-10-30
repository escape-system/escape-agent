//! Skill tools: load a skill, author one, and lint a file (`docs/07`).
//!
//! Loading persists a `skill` message (the prompt uses it to mark the skill as
//! already present) and publishes `session.skill.activated` locally.
//! `skill_write` forces `<scope>/.opencode/skill/<id>/SKILL.md` or the global
//! `<config>/skill/<id>/SKILL.md`, writes atomically, and marks the file with
//! `metadata.opencode/agent: true` so discovery reports
//! [`SkillSource::Agent`]. `skill_lint` is read-only.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use oc_config::{ConfigService, Effect, SkillInfo, SkillSource, parse_markdown};
use oc_store::{NewMessage, StoreError};
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::{EventEnvelope, EventLocation, EventPayload, MessageId, now_millis};
use serde_json::{Map, Value, json};

use crate::runtime::Runtime;

const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 100;
const MAX_SKILL_FILES: usize = 10;
const MAX_SAMPLE_DEPTH: usize = 6;
const SAMPLE_SKIP: [&str; 5] = ["node_modules", ".git", ".venv", "venv", "__pycache__"];

/// `session.skill.activated` (`SessionEvent.Skill.Activated`, durable v1).
#[derive(serde::Serialize)]
struct SkillActivated<'a> {
    #[serde(rename = "sessionID")]
    session_id: &'a str,
    id: &'a str,
    name: &'a str,
    text: &'a str,
}

impl EventPayload for SkillActivated<'_> {
    const TYPE: &'static str = "session.skill.activated";
    const DURABLE: bool = true;
    const VERSION: u32 = 1;
}

/// Build the skill tool set for one runtime.
#[derive(Clone, Debug)]
pub struct SkillTools {
    runtime: Runtime,
}

impl SkillTools {
    #[must_use]
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }

    /// Registry entries: `skill`, `skill_write` and `skill_lint`.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SkillTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(SkillWriteTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(SkillLintTool {
            runtime: self.runtime.clone(),
        }));
        registry
    }
}

struct SkillTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill by id and return its instructions plus a sample of its files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Skill id from the available_skills index."
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let id = required_str(&input, "id")?;
        let config = &self.runtime.inner.config.config;
        let agent = session_agent(&self.runtime, &ctx.session_id).await?;
        if config.evaluate(Some(&agent), "skill", &id) == Effect::Deny {
            return Err(ToolError::new(format!(
                "permission denied for skill `{id}`"
            )));
        }
        let skill = find_skill(config, &id)
            .ok_or_else(|| ToolError::new(format!("unknown skill `{id}`")))?;

        let created = now_millis();
        self.runtime
            .inner
            .config
            .store
            .append_message(NewMessage {
                id: MessageId::new().into_inner(),
                session_id: ctx.session_id.clone(),
                kind: "skill".to_owned(),
                data: json!({
                    "time": { "created": created },
                    "skill": skill.id,
                    "name": skill.name,
                    "text": skill.content,
                }),
                time_created: created,
            })
            .await
            .map_err(|error| store_error(&error))?;

        let directory = ctx.directory.to_string_lossy();
        self.runtime
            .emit(
                &ctx.session_id,
                directory.as_ref(),
                SkillActivated {
                    session_id: &ctx.session_id,
                    id: &skill.id,
                    name: &skill.name,
                    text: &skill.content,
                },
            )
            .await
            .map_err(|error| {
                ToolError::new(format!(
                    "skill was loaded but session.skill.activated could not be persisted: {error}"
                ))
            })?;

        Ok(ToolResult::text(render_skill(&skill)))
    }
}

struct SkillWriteTool {
    runtime: Runtime,
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

struct SkillLintTool {
    runtime: Runtime,
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
            find_skill(config, &id)
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

/// The validated shape of an existing or newly discovered skill, rendered for
/// the model.
fn render_skill(skill: &SkillInfo) -> String {
    let directory = skill
        .path
        .parent()
        .map_or_else(String::new, |path| path.to_string_lossy().into_owned());
    let files = if skill.path.file_name() == Some(OsStr::new("SKILL.md")) {
        sample_files(Path::new(&directory))
    } else {
        Vec::new()
    };
    let mut out = String::with_capacity(skill.content.len() + 256);
    let _ = writeln!(
        out,
        "<skill_content id=\"{}\" name=\"{}\">",
        escape_attr(&skill.id),
        escape_attr(&skill.name)
    );
    let _ = writeln!(out, "# Skill: {}", skill.name);
    out.push('\n');
    out.push_str(skill.content.trim());
    out.push('\n');
    if !directory.is_empty() {
        let _ = writeln!(out, "\nBase directory for this skill: {directory}");
        out.push_str(
            "Relative paths in this skill (e.g., scripts/, reference/) are relative to this base directory.\n",
        );
        out.push_str("Note: file list is sampled.\n\n");
    }
    out.push_str("<skill_files>\n");
    for file in files {
        let _ = writeln!(out, "<file>{file}</file>");
    }
    out.push_str("</skill_files>\n</skill_content>");
    out
}

fn sample_files(directory: &Path) -> Vec<String> {
    let mut files = Vec::new();
    walk_files(directory, 0, &mut files);
    files.sort();
    files.truncate(MAX_SKILL_FILES);
    files
}

fn walk_files(directory: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_SAMPLE_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SAMPLE_SKIP.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk_files(&path, depth + 1, out);
        } else if file_type.is_file() && name != "SKILL.md" {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}

fn find_skill(config: &ConfigService, id: &str) -> Option<SkillInfo> {
    config.skill(id).cloned().or_else(|| {
        config
            .rescan_skills()
            .into_iter()
            .find(|skill| skill.id == id)
    })
}

async fn session_agent(runtime: &Runtime, session: &str) -> Result<String, ToolError> {
    let value = runtime
        .inner
        .config
        .store
        .get_session(session)
        .await
        .map_err(|error| store_error(&error))?;
    let agent = value
        .as_ref()
        .and_then(|value| value.get("agent"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(agent.unwrap_or_else(|| runtime.inner.config.config.default_agent().to_owned()))
}

fn validate_id(id: &str) -> Result<(), String> {
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

fn compose_document(name: &str, description: &str, autoinvoke: Option<bool>, body: &str) -> String {
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

fn skill_id_from_path(path: &Path) -> Option<String> {
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

fn publish(runtime: &Runtime, ctx: &ToolContext, kind: &str, data: Value) {
    let mut envelope = EventEnvelope::now(kind, data);
    envelope.location = Some(EventLocation {
        directory: ctx.directory.to_string_lossy().into_owned(),
    });
    runtime.inner.config.sink.publish(envelope);
}

fn finding(severity: &str, code: &str, message: impl Into<String>) -> Value {
    let mut map = Map::new();
    map.insert("severity".to_owned(), Value::String(severity.to_owned()));
    map.insert("code".to_owned(), Value::String(code.to_owned()));
    map.insert("message".to_owned(), Value::String(message.into()));
    Value::Object(map)
}

fn store_error(error: &StoreError) -> ToolError {
    ToolError::new(error.to_string())
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("`{key}` is required and must be non-empty")))
}

fn optional_str(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_bool(input: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::new(format!("`{key}` must be a boolean"))),
    }
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_the_documented_pattern() {
        assert!(validate_id("rust-build-test").is_ok());
        assert!(validate_id("a1").is_ok());
        assert!(validate_id("A1").is_err());
        assert!(validate_id("a").is_err(), "two characters minimum");
        assert!(validate_id("-a").is_err());
        assert!(validate_id("a_b").is_err());
        assert!(validate_id(&format!("a{}", "b".repeat(64))).is_err());
    }

    #[test]
    fn documents_carry_the_agent_marker_and_quoted_scalars() {
        let document = compose_document(
            "Rust: Build",
            "Use lanes; never raw cargo test",
            Some(true),
            "Body\n",
        );
        assert!(document.starts_with("---\nname: \"Rust: Build\"\n"));
        assert!(
            document.contains(
                "\nmetadata:\n  opencode/agent: true\n  opencode/autoinvoke: true\n---\n"
            )
        );
        let parsed = parse_markdown(&document);
        assert!(parsed.error.is_none());
        assert_eq!(parsed.body.trim(), "Body");
    }

    #[test]
    fn activation_event_is_durable_v1_with_the_documented_fields() {
        let payload = SkillActivated {
            session_id: "ses_one",
            id: "rust-build-test",
            name: "Rust Build",
            text: "body",
        };
        assert_eq!(
            <SkillActivated as EventPayload>::TYPE,
            "session.skill.activated"
        );
        const { assert!(<SkillActivated as EventPayload>::DURABLE) };
        assert_eq!(<SkillActivated as EventPayload>::VERSION, 1);
        assert_eq!(
            payload.data(),
            json!({
                "sessionID": "ses_one",
                "id": "rust-build-test",
                "name": "Rust Build",
                "text": "body",
            })
        );
    }

    #[test]
    fn skill_id_is_derived_like_discovery() {
        assert_eq!(
            skill_id_from_path(Path::new("/x/skill/rust-build-test/SKILL.md")).as_deref(),
            Some("rust-build-test")
        );
        assert_eq!(
            skill_id_from_path(Path::new("/x/skill/loose.md")).as_deref(),
            Some("loose")
        );
    }
}
