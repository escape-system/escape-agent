use std::fmt::Write as _;

use oc_config::{ConfigService, Effect};
use oc_memory::{DEFAULT_TOKEN_BUDGET, MemoryService, RetrieveRequest};

use crate::instructions::{InstructionSource, SourceKind};

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
