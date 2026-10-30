use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::Path;

use oc_config::{ConfigService, Effect, SkillInfo};
use oc_store::NewMessage;
use oc_tools::{Tool, ToolContext, ToolError, ToolResult};
use oc_types::{EventPayload, MessageId, now_millis};
use serde_json::{Value, json};

use crate::runtime::Runtime;

use super::util::{escape_attr, required_str, store_error};
use super::{MAX_SAMPLE_DEPTH, MAX_SKILL_FILES, SAMPLE_SKIP};

/// `session.skill.activated` (`SessionEvent.Skill.Activated`, durable v1).
#[derive(serde::Serialize)]
pub(super) struct SkillActivated<'a> {
    #[serde(rename = "sessionID")]
    pub(super) session_id: &'a str,
    pub(super) id: &'a str,
    pub(super) name: &'a str,
    pub(super) text: &'a str,
}

impl EventPayload for SkillActivated<'_> {
    const TYPE: &'static str = "session.skill.activated";
    const DURABLE: bool = true;
    const VERSION: u32 = 1;
}

pub(super) struct SkillTool {
    pub(super) runtime: Runtime,
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

pub(super) fn find_skill(config: &ConfigService, id: &str) -> Option<SkillInfo> {
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
