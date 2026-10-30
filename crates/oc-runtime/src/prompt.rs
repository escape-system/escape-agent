//! System prompt assembly and transcript conversion.
//!
//! The system prefix is exactly two parts: the agent's `system` string (or the
//! documented default when absent) followed by the rendered instruction
//! builtins. Empty skill and discovery sources are omitted rather than
//! rendered as blank blocks, so the prompt-cache prefix only changes when a
//! real source changes.

use std::fmt::Write as _;

use oc_config::{ConfigService, Effect};
use oc_llm::{ContentPart, LlmMessage, Role};
use oc_memory::{DEFAULT_TOKEN_BUDGET, MemoryService, RetrieveRequest};
use oc_types::ModelRef;
use oc_types::message::{AssistantPart, PromptFileAttachment, ToolContent, ToolState, UserMessage};
use serde_json::Value;

use crate::instructions::{self, InstructionSource, SourceKind};

/// Instruction appended when the step budget is exhausted. The final request
/// is made with tools disabled and asks the model to answer from what it has.
pub const MAX_STEPS_PROMPT: &str = "\
You have reached the step limit for this request. Do not call any more tools. \
Answer the user using the information gathered so far, and state clearly \
anything that remains unverified.";

/// Default system prompt used when the selected agent has no `system` field.
///
/// It advertises the tool names actually offered for this request and embeds
/// the working directory and platform. Rules are deliberately short: tools
/// carry their own descriptions, and long prompts cost cache space.
#[must_use]
pub fn default_system_prompt(tool_names: &[String], directory: &str) -> String {
    default_system_prompt_with_memory(tool_names, directory, false)
}

/// [`default_system_prompt`] with a memory note only when a `<memory>` block is
/// actually part of this request's instruction prefix.
#[must_use]
pub fn default_system_prompt_with_memory(
    tool_names: &[String],
    directory: &str,
    has_memory: bool,
) -> String {
    let tools = if tool_names.is_empty() {
        "none".to_owned()
    } else {
        tool_names.join(", ")
    };
    let mut prompt = format!(
        "You are opencode, an AI coding agent working in the user's terminal.\n\
         \n\
         Help with software engineering tasks: read code, make focused edits, run \
         commands, and explain what you did. Be concise and act on information that is \
         already in the repository instead of asking for it. Never invent file contents, \
         command output, or test results.\n\
         \n\
         Available tools: {tools}.\n\
         \n\
         Rules:\n\
         - Call tools with JSON that matches the provided schema. Every call carries an \
         `expectation` object; it is a contract, not a tool argument.\n\
         - Read a file before editing it. Prefer small edits over rewrites.\n\
         - Use the shell only for commands that genuinely need a shell.\n\
         - When a tool fails, report the failure and try a different approach.\n\
         - Finish with a direct answer; do not describe hidden reasoning.\n"
    );
    if tool_names.iter().any(|name| name == "subagent") {
        prompt.push_str(
            "- When the user explicitly asks you to spawn subagents, call `subagent` before reading \
             files or running commands; give each child a self-contained prompt. Otherwise delegate \
             self-contained parallel work with `subagent` instead of doing everything yourself; omit \
             `agent` to use the configured default, and use `explore` for read-only searches. Several \
             independent spawns may be issued in one step; `background: true` returns immediately and \
             reports the result later as a synthetic message.\n",
        );
    }
    if has_memory {
        prompt.push_str(
            "- Durable notes relevant to this session are included in the `<memory>` \
             instruction source; use them before re-deriving facts, and do not write \
             memory for things the repository already records.\n",
        );
    }
    let _ = write!(
        prompt,
        "\n\
         Environment:\n\
         - working directory: {directory}\n\
         - platform: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    prompt
}

/// The per-request builtin sources: today's date and a compact `<env>` block.
///
/// Slash commands, skills and discovery files are separate sources; the
/// builtins are always present so the prefix never starts empty.
#[must_use]
pub fn builtin_sources(directory: &str) -> Vec<InstructionSource> {
    vec![
        InstructionSource::new(
            SourceKind::Builtins,
            "date",
            format!("Today's date is {} (UTC).", utc_date()),
        ),
        InstructionSource::new(
            SourceKind::Builtins,
            "env",
            format!(
                "<env>\nWorking directory: {directory}\nPlatform: {} ({})\n</env>",
                std::env::consts::OS,
                std::env::consts::ARCH,
            ),
        ),
    ]
}

/// Inputs for one instruction-prefix resolution.
#[derive(Debug, Clone, Copy)]
pub struct InstructionContext<'a> {
    pub config: &'a ConfigService,
    /// Attached memory service, when the host configured one.
    pub memory: Option<&'a MemoryService>,
    pub agent_id: &'a str,
    pub project_id: &'a str,
    pub session_id: &'a str,
    /// Session depth; the session memory scope is visible only below the root.
    pub depth: u32,
    /// Last user text for memory retrieval.
    pub query: &'a str,
    /// Skill ids already loaded in this session.
    pub skill_ids: &'a [String],
    pub directory: &'a str,
    /// Active goal for the session root, when the store has one.
    pub goal: Option<&'a oc_observer::GoalSnapshot>,
}

/// Instruction sources plus the ids of the memories that were rendered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstructionAssembly {
    pub sources: Vec<InstructionSource>,
    /// Memory ids in block order; touched once when the turn succeeds.
    pub memory_ids: Vec<String>,
}

/// Instruction sources for this request in canonical order.
///
/// Discovery files are re-read from the paths `ConfigService::instructions()`
/// recorded so an edited `AGENTS.md` is visible on the next turn without
/// rebuilding the config snapshot. The skill index is re-scanned on every turn
/// for the same reason. Memory retrieval is the only async step; a retrieval
/// failure is logged and the block is omitted rather than failing the turn.
pub async fn instruction_sources(context: &InstructionContext<'_>) -> Vec<InstructionSource> {
    assemble_instructions(context).await.sources
}

/// Resolve every cache-stable instruction source, including memory ids.
pub async fn assemble_instructions(context: &InstructionContext<'_>) -> InstructionAssembly {
    let mut sources = discovery_sources(context.config);
    sources.extend(builtin_sources(context.directory));
    if let Some(skill) = skill_source(context.config, context.agent_id) {
        sources.push(skill);
    }
    if let Some(goal) = context.goal {
        let text = crate::goal::render_goal(goal);
        if !text.trim().is_empty() {
            sources.push(InstructionSource::new(SourceKind::Goal, "oc/goal", text));
        }
    }

    let mut memory_ids = Vec::new();
    if let Some(memory) = context.memory
        && let Some((source, ids)) = memory_source(memory, context).await
    {
        memory_ids = ids;
        sources.push(source);
    }

    sources.sort_by(|left, right| {
        (left.kind, left.key.as_str()).cmp(&(right.kind, right.key.as_str()))
    });
    InstructionAssembly {
        sources,
        memory_ids,
    }
}

/// The two-part system prefix: base prompt, then the builtin instructions.
///
/// Kept for callers that only need the builtins; the runtime uses
/// [`system_prefix`] with the full frozen instruction set.
#[must_use]
pub fn system_parts(
    agent_system: Option<&str>,
    tool_names: &[String],
    directory: &str,
) -> Vec<String> {
    let instructions = instructions::render(&builtin_sources(directory));
    system_prefix(agent_system, tool_names, directory, false, &instructions)
}

/// The two-part system prefix with a frozen instruction prefix.
#[must_use]
pub fn system_prefix(
    agent_system: Option<&str>,
    tool_names: &[String],
    directory: &str,
    has_memory: bool,
    instructions: &str,
) -> Vec<String> {
    let base = agent_system.map_or_else(
        || default_system_prompt_with_memory(tool_names, directory, has_memory),
        str::to_owned,
    );
    if instructions.trim().is_empty() {
        vec![base]
    } else {
        vec![base, instructions.to_owned()]
    }
}

fn discovery_sources(config: &ConfigService) -> Vec<InstructionSource> {
    config
        .instructions()
        .iter()
        .map(|file| {
            let text = std::fs::read_to_string(&file.path).unwrap_or_else(|_| file.text.clone());
            InstructionSource::new(
                SourceKind::Discovery,
                format!("discovery:{}", file.path.to_string_lossy()),
                text,
            )
        })
        .collect()
}

fn skill_source(config: &ConfigService, agent_id: &str) -> Option<InstructionSource> {
    // Re-scan roots so a skill written after config load is visible on the
    // next execution; fall back to the loaded snapshot when scanning finds
    // nothing (e.g. an in-memory test config).
    let mut current = config.rescan_skills();
    if current.is_empty() {
        current = config.skills().to_vec();
    }
    let mut skills: Vec<_> = current
        .into_iter()
        .filter(|skill| skill.description.is_some())
        .filter(|skill| skill.autoinvoke != Some(false))
        .filter(|skill| config.evaluate(Some(agent_id), "skill", &skill.id) != Effect::Deny)
        .collect();
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    if skills.is_empty() {
        return None;
    }

    let mut text = String::from(
        "Skills provide specialized instructions and workflows for specific tasks.\n\
         Use the skill tool to load a skill when a task matches its description.\n\
         \n\
         <available_skills>",
    );
    for skill in skills {
        let description = skill.description.as_deref().unwrap_or_default();
        let _ = write!(
            text,
            "\n  <skill id=\"{}\" name=\"{}\" description=\"{}\"/>",
            escape_xml(&skill.id),
            escape_xml(&skill.name),
            escape_xml(description),
        );
    }
    text.push_str("\n</available_skills>");
    Some(InstructionSource::new(
        SourceKind::Skills,
        "core/skill-guidance",
        text,
    ))
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            other => escaped.push(other),
        }
    }
    escaped
}

async fn memory_source(
    memory: &MemoryService,
    context: &InstructionContext<'_>,
) -> Option<(InstructionSource, Vec<String>)> {
    let mut scopes = vec![
        ("global".to_owned(), None),
        ("project".to_owned(), Some(context.project_id.to_owned())),
        ("agent".to_owned(), Some(context.agent_id.to_owned())),
    ];
    if context.depth > 0 {
        scopes.push(("session".to_owned(), Some(context.session_id.to_owned())));
    }

    let request = RetrieveRequest {
        scopes,
        query: memory_query(context.query, context.skill_ids),
        kinds: Vec::new(),
        tags: Vec::new(),
        limit: 8,
        token_budget: DEFAULT_TOKEN_BUDGET,
    };
    match memory.retrieve(request).await {
        Ok(block) if !block.text.trim().is_empty() => Some((
            InstructionSource::new(SourceKind::Memory, "oc/memory", block.text),
            block.ids,
        )),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(error = %error, "memory retrieval failed; omitting the memory block");
            None
        }
    }
}

fn memory_query(last_user: &str, skill_ids: &[String]) -> String {
    let mut query: String = last_user.trim().chars().take(400).collect();
    let trimmed = query.trim_end().len();
    query.truncate(trimmed);
    if !skill_ids.is_empty() {
        if !query.is_empty() {
            query.push(' ');
        }
        query.push_str(&skill_ids.join(" "));
    }
    query
}

/// Convert stored wire messages into provider messages.
///
/// The last completed compaction is the context boundary: its summary becomes
/// the first user message and everything before it is dropped, except the
/// retained tail named by `keepID`. Assistant tool calls are replayed with
/// their results so the model sees why a tool ran.
#[must_use]
pub fn messages_from_store(messages: &[Value]) -> Vec<LlmMessage> {
    let boundary = messages.iter().rposition(|message| {
        message_type(message) == Some("compaction") && compaction_completed(message)
    });
    let mut out = Vec::new();
    let mut start = 0;
    if let Some(index) = boundary {
        if let Some(summary) = messages[index].get("summary").and_then(Value::as_str) {
            out.push(LlmMessage::user(summary));
        }
        if let Some(keep_id) = messages[index].get("keepID").and_then(Value::as_str)
            && let Some(keep) = messages[..index]
                .iter()
                .position(|message| message.get("id").and_then(Value::as_str) == Some(keep_id))
        {
            push_messages(&messages[keep..index], &mut out);
        }
        start = index + 1;
    }
    push_messages(&messages[start..], &mut out);
    out
}

fn push_messages(messages: &[Value], out: &mut Vec<LlmMessage>) {
    for message in messages {
        match message_type(message) {
            Some("user") => {
                out.push(LlmMessage::new(Role::User, user_parts(message)));
            }
            Some("synthetic" | "system" | "skill") => {
                if let Some(text) = message.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(LlmMessage::user(text.to_owned()));
                }
            }
            Some("peer") => {
                let text = message
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !text.is_empty() {
                    let from = message.get("from").and_then(Value::as_str).unwrap_or("?");
                    let agent = message.get("agent").and_then(Value::as_str).unwrap_or("?");
                    let kind = message
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("note");
                    out.push(LlmMessage::user(format!(
                        "<peer from=\"{from}\" agent=\"{agent}\" kind=\"{kind}\">{text}</peer>"
                    )));
                }
            }
            Some("assistant") => assistant_messages(message, out),
            Some(_) | None => {}
        }
    }
}

fn user_parts(message: &Value) -> Vec<ContentPart> {
    let mut parts: Vec<ContentPart> = Vec::new();
    if let Ok(user) = serde_json::from_value::<UserMessage>(message.clone()) {
        if !user.text.is_empty() {
            parts.push(ContentPart::text(user.text));
        }
        if let Some(files) = user.files {
            for file in files {
                parts.extend(attachment_parts(&file));
            }
        }
    } else if let Some(text) = message.get("text").and_then(Value::as_str)
        && !text.is_empty()
    {
        parts.push(ContentPart::text(text.to_owned()));
    }
    if parts.is_empty() {
        parts.push(ContentPart::text(String::new()));
    }
    parts
}

fn attachment_parts(file: &PromptFileAttachment) -> Vec<ContentPart> {
    let data = strip_data_url(&file.data);
    if file.mime.starts_with("image/") {
        return vec![ContentPart::image(file.mime.clone(), data)];
    }
    if file.mime == "application/pdf" {
        return vec![ContentPart::document(
            file.mime.clone(),
            data,
            file.name.clone(),
        )];
    }
    let name = file.name.as_deref().unwrap_or("attachment");
    vec![ContentPart::text(format!(
        "[attached file: {name} ({}), {} bytes]",
        file.mime,
        data.len()
    ))]
}

fn strip_data_url(data: &str) -> String {
    if let Some((_, rest)) = data.split_once(',')
        && data.starts_with("data:")
    {
        return rest.to_owned();
    }
    data.to_owned()
}

fn assistant_messages(message: &Value, out: &mut Vec<LlmMessage>) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    let Ok(parts) = serde_json::from_value::<Vec<AssistantPart>>(Value::Array(content.clone()))
    else {
        return;
    };
    let mut calls: Vec<ContentPart> = Vec::new();
    let mut results: Vec<LlmMessage> = Vec::new();
    for part in parts {
        match part {
            AssistantPart::Text(text) => {
                if !text.text.is_empty() {
                    calls.push(ContentPart::text(text.text));
                }
            }
            AssistantPart::Reasoning(_) => {}
            AssistantPart::Tool(tool) => match &tool.state {
                ToolState::Completed { input, content, .. } => {
                    calls.push(ContentPart::tool_call(
                        tool.id.clone(),
                        tool.name.clone(),
                        Value::Object(input.clone()),
                    ));
                    results.push(LlmMessage::tool_result(
                        tool.id.clone(),
                        Some(tool.name.clone()),
                        join_content(content),
                        false,
                    ));
                }
                ToolState::Error {
                    input,
                    error,
                    content,
                    ..
                } => {
                    calls.push(ContentPart::tool_call(
                        tool.id.clone(),
                        tool.name.clone(),
                        Value::Object(input.clone()),
                    ));
                    let text = content
                        .as_ref()
                        .map(|content| join_content(content))
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| error.message.clone());
                    results.push(LlmMessage::tool_result(
                        tool.id.clone(),
                        Some(tool.name.clone()),
                        text,
                        true,
                    ));
                }
                ToolState::Running { input, .. } => {
                    calls.push(ContentPart::tool_call(
                        tool.id.clone(),
                        tool.name.clone(),
                        Value::Object(input.clone()),
                    ));
                }
                ToolState::Streaming { .. } => {}
            },
        }
    }
    if !calls.is_empty() {
        out.push(LlmMessage::new(Role::Assistant, calls));
    }
    out.extend(results);
}

fn join_content(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|part| match part {
            ToolContent::Text { text } => text.clone(),
            ToolContent::File { uri, mime, .. } => format!("[file {mime}: {uri}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_type(message: &Value) -> Option<&str> {
    message.get("type").and_then(Value::as_str)
}

/// Last user message text in a provider transcript.
#[must_use]
pub fn last_user_text(messages: &[LlmMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn compaction_completed(message: &Value) -> bool {
    message.get("status").and_then(Value::as_str) == Some("completed")
        && message
            .get("summary")
            .and_then(Value::as_str)
            .is_some_and(|summary| !summary.trim().is_empty())
}

/// Parse a model reference carried as a string or `{providerID,id,variant}`.
#[must_use]
pub fn model_ref(value: &Value) -> Option<ModelRef> {
    match value {
        Value::String(text) => parse_reference(text),
        Value::Object(object) => {
            let provider = object.get("providerID").and_then(Value::as_str)?;
            let id = object
                .get("id")
                .or_else(|| object.get("model"))
                .and_then(Value::as_str)?;
            Some(ModelRef {
                id: id.to_owned(),
                provider_id: provider.to_owned(),
                variant: object
                    .get("variant")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        _ => None,
    }
}

fn parse_reference(text: &str) -> Option<ModelRef> {
    let (reference, variant) = match text.split_once('#') {
        Some((reference, variant)) => (reference, Some(variant.to_owned())),
        None => (text, None),
    };
    let (provider, id) = reference.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some(ModelRef {
        id: id.to_owned(),
        provider_id: provider.to_owned(),
        variant,
    })
}

fn utc_date() -> String {
    let days = oc_types::now_millis().div_euclid(86_400_000);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_prompt_lists_tools_and_directory() {
        let tools = vec!["read".to_owned(), "shell".to_owned()];
        let prompt = default_system_prompt(&tools, "/work");
        assert!(prompt.contains("read, shell"));
        assert!(prompt.contains("/work"));
        assert!(prompt.contains("expectation"));
    }

    #[test]
    fn delegation_guidance_appears_only_with_the_subagent_tool() {
        let with = default_system_prompt(&["read".to_owned(), "subagent".to_owned()], "/work");
        assert!(with.contains("`subagent`"), "{with}");
        assert!(with.contains("`explore`"), "{with}");
        assert!(with.contains("background: true"), "{with}");
        let without = default_system_prompt(&["read".to_owned()], "/work");
        assert!(!without.contains("`subagent`"), "{without}");
        assert!(!without.contains("background: true"), "{without}");
    }

    #[test]
    fn system_prefix_is_two_parts() {
        let parts = system_parts(Some("custom"), &[], "/work");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "custom");
        assert!(parts[1].contains("<env>"));
    }

    #[test]
    fn compaction_summary_is_the_context_boundary() {
        let messages = vec![
            json!({"type": "user", "text": "old"}),
            json!({"type": "compaction", "status": "completed", "summary": "handoff"}),
            json!({"type": "user", "text": "new"}),
        ];
        let converted = messages_from_store(&messages);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].content[0].as_text(), Some("handoff"));
        assert_eq!(converted[1].content[0].as_text(), Some("new"));
    }

    #[test]
    fn compaction_keep_id_replays_the_retained_tail() {
        let messages = vec![
            json!({"id": "m1", "type": "user", "text": "old"}),
            json!({"id": "m2", "type": "user", "text": "keep"}),
            json!({"id": "m3", "type": "assistant", "time": {"created": 1, "completed": 2},
                   "content": [{"type": "text", "text": "answer"}]}),
            json!({"id": "c1", "type": "compaction", "status": "completed",
                   "summary": "handoff", "keepID": "m2"}),
            json!({"id": "m4", "type": "user", "text": "new"}),
        ];
        let converted = messages_from_store(&messages);
        let texts: Vec<&str> = converted
            .iter()
            .map(|message| message.content[0].as_text().unwrap_or_default())
            .collect();
        assert_eq!(texts, vec!["handoff", "keep", "answer", "new"]);
    }

    #[test]
    fn assistant_tool_calls_are_replayed_with_results() {
        let messages = vec![json!({
            "type": "assistant",
            "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool", "id": "call_1", "name": "read",
                 "state": {"status": "completed", "input": {"path": "a"},
                           "content": [{"type": "text", "text": "body"}]},
                 "time": {"created": 1}}
            ]
        })];
        let converted = messages_from_store(&messages);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].role, Role::Assistant);
        assert_eq!(converted[1].role, Role::Tool);
    }

    #[test]
    fn model_references_parse_both_shapes() {
        assert_eq!(
            model_ref(&json!("test/alpha#fast")),
            Some(ModelRef {
                provider_id: "test".to_owned(),
                id: "alpha".to_owned(),
                variant: Some("fast".to_owned()),
            })
        );
        assert_eq!(
            model_ref(&json!({"providerID": "test", "id": "alpha"})),
            Some(ModelRef {
                provider_id: "test".to_owned(),
                id: "alpha".to_owned(),
                variant: None,
            })
        );
    }
}
