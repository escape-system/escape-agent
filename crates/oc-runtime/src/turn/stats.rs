use oc_types::message::{AssistantPart, ToolContent, ToolState};

use super::assistant::AssistantState;

/// Everything the turn observed that the scribe digest needs.
#[derive(Debug, Default)]
pub(super) struct TurnStats {
    pub(super) tool_calls: usize,
    pub(super) user_text: String,
    pub(super) assistant_text: String,
    pub(super) tool_lines: Vec<String>,
}

pub(super) fn collect_tool_lines(stats: &mut TurnStats, message: &AssistantState) {
    for part in &message.content {
        let AssistantPart::Tool(tool) = part else {
            continue;
        };
        let line = match &tool.state {
            ToolState::Completed { content, .. } => {
                let text = tool_text(content);
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
            ToolContent::File { uri, mime, .. } => format!("[file {mime}: {uri}]"),
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
