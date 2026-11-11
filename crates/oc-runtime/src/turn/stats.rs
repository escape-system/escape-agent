use oc_types::message::{AssistantPart, ToolContent, ToolState};
use serde_json::Value;

use super::assistant::AssistantState;
use super::verify;

/// Everything the turn observed that the scribe digest needs.
#[derive(Debug, Default)]
pub(super) struct TurnStats {
    pub(super) tool_calls: usize,
    pub(super) user_text: String,
    pub(super) assistant_text: String,
    pub(super) tool_lines: Vec<String>,
    pub(super) changed_code_files: Vec<String>,
    pub(super) last_code_mutation_step: Option<usize>,
    pub(super) last_verification_step: Option<usize>,
    pub(super) verify_nudges: usize,
}

impl TurnStats {
    pub(super) fn record_code_mutation(&mut self, path: &str, step: usize) {
        if !self.changed_code_files.iter().any(|p| p == path) {
            self.changed_code_files.push(path.to_owned());
        }
        self.last_code_mutation_step = Some(step);
    }

    pub(super) fn record_verification(&mut self, step: usize) {
        self.last_verification_step = Some(step);
    }
}

pub(super) fn collect_tool_lines(
    stats: &mut TurnStats,
    message: &AssistantState,
    step_number: usize,
) {
    for part in &message.content {
        let AssistantPart::Tool(tool) = part else {
            continue;
        };
        let line = match &tool.state {
            ToolState::Completed { input, content, .. } => {
                let text = tool_text(content);
                match tool.name.as_str() {
                    "write" | "edit" => {
                        if let Some(path) = input.get("path").and_then(Value::as_str)
                            && verify::is_code_file(path)
                        {
                            stats.record_code_mutation(path, step_number);
                        }
                    }
                    "patch" => {
                        if let Some(patch_text) = input.get("patchText").and_then(Value::as_str) {
                            for target in oc_tools::patch::patch_targets(patch_text) {
                                if verify::is_code_file(&target) {
                                    stats.record_code_mutation(&target, step_number);
                                }
                            }
                        }
                    }
                    "shell" => {
                        if let Some(command) = input.get("command").and_then(Value::as_str)
                            && verify::is_verification_command(command)
                        {
                            stats.record_verification(step_number);
                        }
                    }
                    _ => {}
                }
                format!("- {}: ok{}", tool.name, tool_summary(&text))
            }
            ToolState::Error { error, .. } => {
                format!("- {}: error: {}", tool.name, error.message)
            }
            ToolState::Streaming { .. } | ToolState::Running { .. } => continue,
        };
        stats.tool_lines.push(line);
    }
}

pub(super) fn tool_text(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|part| match part {
            ToolContent::Text { text } => text.clone(),
            ToolContent::File { uri, mime, name } => {
                if uri.starts_with("data:") {
                    let payload_len = uri
                        .split_once(',')
                        .map_or(uri.len(), |(_, rest)| rest.len());
                    let approx_bytes = payload_len * 3 / 4;
                    match name.as_deref().filter(|name| !name.is_empty()) {
                        Some(name) => format!("[file {mime} ({name}): {approx_bytes} bytes]"),
                        None => format!("[file {mime}: {approx_bytes} bytes]"),
                    }
                } else {
                    format!("[file {mime}: {uri}]")
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn tool_summary(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        return String::new();
    }
    let summary: String = first.chars().take(160).collect();
    format!(" ({summary})")
}

pub(super) fn scribe_digest(stats: &TurnStats) -> String {
    let user: String = stats.user_text.trim().chars().take(1500).collect();
    let assistant: String = stats.assistant_text.trim().chars().take(2000).collect();
    let tools = if stats.tool_lines.is_empty() {
        "(none)".to_owned()
    } else {
        stats.tool_lines.join("\n")
    };
    format!(
        "User intent:\n{user}\n\n\
         Assistant final text:\n{assistant}\n\n\
         Tool outcomes:\n{tools}"
    )
}
