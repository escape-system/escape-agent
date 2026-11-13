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
            ToolState::Completed {
                input,
                content,
                metadata,
                ..
            } => {
                let text = tool_text(content);
                let shell_failure = if tool.name == "shell" {
                    verify::shell_failure_reason(metadata.as_ref())
                } else {
                    None
                };
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
                    "symbol_edit" => {
                        let input_val = Value::Object(input.clone());
                        if !crate::snapshot::is_symbol_edit_preview(&input_val) {
                            let mut recorded = false;
                            if let Some(files) = metadata
                                .as_ref()
                                .and_then(|m| m.get("files"))
                                .and_then(Value::as_array)
                            {
                                for entry in files {
                                    if let Some(file) = entry.get("file").and_then(Value::as_str) {
                                        recorded = true;
                                        if verify::is_code_file(file) {
                                            stats.record_code_mutation(file, step_number);
                                        }
                                    }
                                }
                            }
                            if !recorded
                                && let Some(path) =
                                    crate::snapshot::symbol_edit_target_path(&input_val)
                                && verify::is_code_file(&path)
                            {
                                stats.record_code_mutation(&path, step_number);
                            }
                        }
                    }
                    "shell" => {
                        if shell_failure.is_none()
                            && let Some(command) = input.get("command").and_then(Value::as_str)
                            && verify::is_verification_command(command)
                        {
                            stats.record_verification(step_number);
                        }
                    }
                    _ => {}
                }
                if let Some(reason) = shell_failure {
                    format!("- {}: error: {reason}{}", tool.name, tool_summary(&text))
                } else {
                    format!("- {}: ok{}", tool.name, tool_summary(&text))
                }
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

#[cfg(test)]
mod tests {
    use oc_types::message::{ToolPart, ToolTime};
    use oc_types::{MessageId, ModelRef};
    use serde_json::{Map, json};

    use super::*;

    fn shell_step(command: &str, metadata: Option<Map<String, Value>>) -> AssistantState {
        let mut input = Map::new();
        input.insert("command".to_owned(), json!(command));
        let mut state = AssistantState::new(
            MessageId::from_wire("msg_1"),
            "build".to_owned(),
            ModelRef {
                provider_id: "test".to_owned(),
                id: "alpha".to_owned(),
                variant: None,
            },
            1,
        );
        state.content.push(AssistantPart::Tool(Box::new(ToolPart {
            id: "call_1".to_owned(),
            name: "shell".to_owned(),
            executed: Some(true),
            native: None,
            time: ToolTime {
                created: 1,
                ran: Some(1),
                completed: Some(2),
            },
            state: ToolState::Completed {
                input,
                content: vec![ToolContent::Text {
                    text: "test output".to_owned(),
                }],
                metadata,
            },
        })));
        state
    }

    #[test]
    fn failed_shell_verification_does_not_mark_step_verified() {
        let mut stats = TurnStats::default();
        let mut fail_meta = Map::new();
        fail_meta.insert("status".to_owned(), json!("completed"));
        fail_meta.insert("exit".to_owned(), json!(101));

        collect_tool_lines(&mut stats, &shell_step("cargo test", Some(fail_meta)), 1);
        assert_eq!(stats.last_verification_step, None);
        assert!(
            stats
                .tool_lines
                .iter()
                .any(|line| line.contains("exited with code 101")),
            "{:?}",
            stats.tool_lines
        );

        let mut ok_meta = Map::new();
        ok_meta.insert("status".to_owned(), json!("completed"));
        ok_meta.insert("exit".to_owned(), json!(0));
        collect_tool_lines(&mut stats, &shell_step("cargo test", Some(ok_meta)), 2);
        assert_eq!(stats.last_verification_step, Some(2));
    }
}
