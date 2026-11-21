//! System prompt assembly and transcript conversion.
//!
//! The system prefix is exactly two parts: the agent's `system` string (or the
//! documented default when absent) followed by the rendered instruction
//! builtins. Empty skill and discovery sources are omitted rather than
//! rendered as blank blocks, so the prompt-cache prefix only changes when a
//! real source changes.

use std::fmt::Write as _;

use crate::instructions;

mod model;
mod sources;
mod transcript;

#[cfg(test)]
mod tests;

pub use model::model_ref;
#[cfg(test)]
pub(crate) use sources::skill_source;
pub use sources::{
    InstructionAssembly, InstructionContext, assemble_instructions, builtin_sources,
    instruction_sources,
};
pub use transcript::{last_user_text, messages_from_store};

/// Instruction appended when the step budget is exhausted. The final request
/// is made with tools disabled and asks the model to answer from what it has.
pub const MAX_STEPS_PROMPT: &str = "\
You have reached the step limit for this request. Do not call any more tools. \
Answer the user using the information gathered so far, and state clearly \
anything that remains unverified. The deterministic budget policy was consulted \
and denied an extension this turn; do not promise another retry.";

/// Early-warning notice appended while tool-enabled steps remain. Unlike
/// [`MAX_STEPS_PROMPT`] this does NOT disable tools: the agent may finish
/// verification, or request a bounded extension through `goal_update` and the
/// observer (`repair.request_budget_extension`). Must be sent at every
/// `stepsRemaining <= 5` boundary, not only at exactly 5 or 1.
pub const STEP_BUDGET_WARNING: &str = "\
Step budget notice: tool-enabled steps are running low. Finish pending \
verification or commits now, or request a bounded extension via `goal_update` \
and the observer (`repair.request_budget_extension`). Tools remain enabled.";

/// Instruction appended when a step finishes with `FinishReason::Length` and no
/// tool calls so the model can finish a truncated response cleanly.
pub const CONTINUE_AFTER_OUTPUT_LIMIT_PROMPT: &str = "\
Your previous response was cut off because it hit the output token limit. \
Continue directly from where you stopped without repeating prior text.";

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
    append_capability_routing(&mut prompt, tool_names, has_memory);
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

fn has_tool(tool_names: &[String], name: &str) -> bool {
    tool_names.iter().any(|tool| tool == name)
}

fn has_any_tool(tool_names: &[String], names: &[&str]) -> bool {
    names.iter().any(|name| has_tool(tool_names, name))
}

fn append_capability_routing(prompt: &mut String, tool_names: &[String], has_memory: bool) {
    append_coordination_and_memory_routing(prompt, tool_names, has_memory);
    append_workspace_and_workflow_routing(prompt, tool_names);
}

fn append_coordination_and_memory_routing(
    prompt: &mut String,
    tool_names: &[String],
    has_memory: bool,
) {
    if has_tool(tool_names, "subagent") {
        prompt.push_str(
            "- When the user explicitly asks you to spawn subagents, call `subagent` before reading \
             files or running commands; give each child a self-contained prompt. Otherwise delegate \
             self-contained parallel work with `subagent` instead of doing everything yourself; omit \
             `agent` to use the configured default, and use `explore` for read-only searches. Pass \
             `partitions` (2..16) to auto-split cross-module tasks using `code_map` graph partitioning \
             so parallel workers stay disjoint with minimal `<shared_core>` overhead. For complex \
             multi-phase work, delegate to a coordinator (`role: \"coordinator\"`) to manage sub-tasks \
             hierarchically. Use `sessionID` to continue an existing child rather than spawning duplicates. \
             Several independent spawns may be issued in one step; `background: true` returns immediately \
             and reports the result later as a synthetic message.\n",
        );
    }
    if has_any_tool(tool_names, &["talk", "roster", "session_write", "digest"]) {
        let mut parts = Vec::new();
        if has_tool(tool_names, "roster") {
            parts.push(
                "`roster` (`wait_seconds` blocks until child state changes; never poll with sleep)",
            );
        }
        if has_tool(tool_names, "talk") {
            parts.push("`talk` (send/request asynchronous peer or parent/coordinator messages)");
        }
        if has_tool(tool_names, "session_write") {
            parts.push("`session_write` (`delivery=\"queue\"` queues or wakes an idle child; `delivery=\"steer\"` interrupts/injects urgently mid-turn)");
        }
        if has_tool(tool_names, "digest") {
            parts.push("`digest` (summarize a child session transcript compactly)");
        }
        let _ = writeln!(prompt, "- Subagent coordination: {}.", parts.join("; "));
    }
    if has_any_tool(
        tool_names,
        &["goal_set", "goal_update", "goal_complete", "goal_status"],
    ) {
        prompt.push_str(
            "- When working under an active goal, call `goal_complete` with evidence references \
             (tool or message IDs) when finishing a criterion, or `goal_update` to record progress \
             for the observer; if a task genuinely needs more steps, request a bounded extension via \
             `goal_update` and the observer (`request_budget_extension`).\n",
        );
    }
    if has_any_tool(
        tool_names,
        &[
            "box_list",
            "box_read",
            "box_write",
            "box_edit",
            "box_search",
            "box_meta",
            "box_delete",
        ],
    ) {
        prompt.push_str(
            "- Context boxes (`box_*`) are session-only working state (`<boxes>` index in your \
             context): `scope=\"tree\"` (default) shares across your parent/subagent tree while \
             `scope=\"session\"` keeps private worker scratchpads without collisions; `stable` survives \
             compactions while `temporary` expires at the next one. Check `box_list`/`box_search`/`box_read` \
             before re-deriving shared notes; prefer `box_edit` for small updates and `box_meta` to \
             rename, switch `kind`/`scope`, or revive an expired box.\n",
        );
    }
    if has_memory {
        prompt.push_str(
            "- Permanent memory (`<memory>` / `memory_*`) is cross-session knowledge (project facts, \
             decisions, preferences, gotchas <=2000 chars); use `<memory>` before re-deriving facts, \
             never write repo-derivable facts or single-session scratch/plans into `memory_write` (use `box_write` for session state).\n",
        );
    } else if has_any_tool(
        tool_names,
        &["memory_write", "memory_search", "memory_list"],
    ) {
        prompt.push_str(
            "- Permanent memory (`memory_*`) is cross-session knowledge (project facts, \
             decisions, preferences, gotchas <=2000 chars); never write repo-derivable facts or \
             single-session scratch/plans into `memory_write` (use `box_write` for session state).\n",
        );
    }
}

fn append_workspace_and_workflow_routing(prompt: &mut String, tool_names: &[String]) {
    if has_tool(tool_names, "code_map") {
        prompt.push_str(
            "- Prefer `code_map` FIRST before repeated `grep` or whole-file `read` calls:\n  \
               - Start unfamiliar tasks with `code_map(operation=\"pack_task\", query=...)` (or add `partitions=N` for multi-agent slices), or `communities` / `map` for module hubs and architecture.\n  \
               - Use `symbol` + `expand` (with `compress: true` to strip comments) instead of reading whole files, `impact` before modifying shared symbols, `connect` / `trace` / `affected` to trace call paths, stack traces, or impacted tests, and `batch` for up to 16 lookups in one call.\n  \
               - For deeper analysis use `lego` (trait/interface contracts), `slice` (data/variable flow), `cochange` (files historically edited together), `doc_drift`, `flags`, and `quality` (`delta`/`dead_code`/`clones`/`hotspots`/`deps`/`seams`); pass `precision: \"scip\" | \"lsp\" | \"hybrid\"` when compiler-exact typing is needed.\n",
        );
    }
    if has_tool(tool_names, "symbol_edit") {
        prompt.push_str(
            "- Prefer `symbol_edit` (`replace`, `insert_before`, `insert_after`, `preview`) over brittle text matching when replacing or annotating whole functions, structs, methods, or classes by symbol name; act on any `<lsp_diagnostics>` or `<post_edit_radar>` surfaced after edits.\n",
        );
    }
    if has_tool(tool_names, "lane") {
        prompt.push_str(
            "- Build & test serialization (`lane`): inspect `status` (or `list`) for active/queued \
             tickets before submitting duplicate builds; `queue` submits a ticket and auto-computes \
             `tree_rev` for cargo coalescing; `wait` waits and returns trailing output; `order`/`cancel`/`notify` steer execution.\n",
        );
    } else if has_tool(tool_names, "shell") {
        prompt.push_str(
            "- Build and test commands (`cargo`, `npm`/`pnpm`/`bun`/`yarn`, `pytest`, `go test`, etc.) \
             run through workspace lanes automatically so concurrent builds serialize cleanly without lock fights.\n",
        );
    }
    if has_tool(tool_names, "skill") {
        prompt.push_str(
            "- Load matching project/workflow skills with `skill` before repeating multi-step procedures.\n",
        );
    }
    if has_tool(tool_names, "skill_write") {
        prompt.push_str(
            "- When you discover or complete a repeatable multi-step engineering workflow or \
             project-specific convention that future sessions will repeat, persist it as a reusable \
             skill using `skill_write`. Keep bodies under 200 lines and include a '## When to use' section.\n",
        );
    }
    if has_tool(tool_names, "compact_context") {
        prompt.push_str(
            "- You may compact your own session by calling `compact_context` with a short `note` \
             saying why. It does not run inline: the request is queued and the compaction happens \
             at the next turn boundary, after this turn. Call it when the conversation is roughly \
             60-70% of the model's context window, or before starting a long multi-step task.\n",
        );
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
    // An agent with its own system prompt bypasses
    // `default_system_prompt_with_memory`, so the memory note must still be
    // appended here: the `<memory>` instruction source exists for every agent,
    // not only for the built-in default prompt.
    let base = match agent_system {
        Some(agent) => {
            let mut extra = String::new();
            append_capability_routing(&mut extra, tool_names, has_memory);
            if extra.is_empty() && tool_names.is_empty() {
                agent.to_owned()
            } else {
                let mut combined = agent.trim_end().to_owned();
                if !tool_names.is_empty() {
                    let tools = tool_names.join(", ");
                    let _ = write!(
                        combined,
                        "\n\nAvailable tools: {tools}.\n\
                         - Every tool schema includes an `expectation` contract (`why`, \
                         `expect_seconds`, `intervene_after_seconds`); pass realistic estimates."
                    );
                }
                if !extra.is_empty() {
                    combined.push('\n');
                    combined.push_str(&extra);
                }
                combined
            }
        }
        None => default_system_prompt_with_memory(tool_names, directory, has_memory),
    };
    if instructions.trim().is_empty() {
        vec![base]
    } else {
        vec![base, instructions.to_owned()]
    }
}
