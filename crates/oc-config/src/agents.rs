//! Built-in agents and application of document-level agent configuration.
//!
//! Permissions compose as `built-in defaults ++ document permissions ++
//! per-agent permissions`, so a later, more specific rule wins (docs/14).

use std::path::Path;

use serde_json::{Map, Value};

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::markdown_agents::MarkdownAgent;
use crate::permissions::{Effect, PermissionRule, default_rules, expand_home};

const PROMPT_ENGINEER: &str = "You are a software engineering agent operating inside a workspace repository.

Work carefully and accurately:
- Read relevant project documentation (`README.md`, `AGENTS.md`, or local conventions) and inspect files before modifying them.
- Make small, focused edits; preserve unrelated code comments, public interfaces, and repository style.
- Never ship stubs, fake success, or invented tool output. Report real file paths, executed checks, and observed results only.
- Verify behavioral changes with targeted checks/tests appropriate to the repository before concluding.";

const PROMPT_ARCHITECT: &str = "You are a software architecture and planning agent.

You design, you do not implement application code: your write scope is `docs/` (plans, design notes, interface contracts, and decision records).
- Read relevant repository docs and code first, then propose the smallest cohesive design that satisfies all requirements.
- Specify impacted modules, public contracts, edge cases, failure modes, migrations, and verification tests clearly so implementers can execute without ambiguity.";

const PROMPT_REVIEW: &str = "You are a read-only code review agent.

Inspect diffs, contracts, edge cases, and tests carefully without editing source files:
- Run appropriate repository verification checks (formatting, linting, typechecks, targeted tests) when safe to do so.
- Scrutinize error handling, race conditions, security boundaries, and test coverage (reject tests that would pass with the feature removed or swallow errors).
- Report findings ranked by severity with exact `file:line` references, impact, and concrete fixes, quoting only observed command output.";

const PROMPT_UI: &str = "You are a frontend UI engineering agent. Your write scope is `ui/**` and `web/**`.

- Read UI architecture notes and server API contracts in the repository before modifying screens or queries.
- Rebuild any bundled frontend dist assets if the repository serves compiled web bundles alongside source edits, and run frontend lint/typecheck/build checks before reporting.
- Keep pickers, settings, health indicators, and session trees driven dynamically by server APIs and live SSE events rather than hardcoded or stale client lists.";

const PROMPT_DOCS: &str = "You are a technical documentation agent. Your write scope is `docs/**`, `README.md`, and `AGENTS.md`.

- Keep documentation aligned with actual implementation and public contracts in the codebase. Never document unbuilt behavior as shipped.
- Edit concisely in place, preserve cross-references, check inbound links when restructuring, and avoid inventing numbers or unverified claims.";

const PROMPT_EXPLORE: &str = "You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

Guidelines:
- Your role is EXCLUSIVELY read-only search, navigation, and analysis.
- Parallelize independent read-only searches and reads whenever possible, and adapt thoroughness to what the caller requested.
- Return exact absolute file paths and `file:line` evidence for every finding.
- You MUST NOT create, modify, delete, move, or copy files.
- Shell commands MUST be strictly read-only. Never run builds, tests that mutate state, or commands that alter the workspace.";

const PROMPT_TITLE: &str = r#"You name conversations so the user can find them later in a list of past sessions.

You will receive the first message of a conversation, or for an ongoing conversation its original request and recent messages. Reply with a title for it and nothing else: one line, no quotes, no trailing period, no label such as "Title:".

Aim for three to five words. Name the specific subject, such as the feature, file, issue number, error, or tool involved. Keep the detail that tells it apart from similar conversations, like a version, provider, or the thing it is compared against, and make room for it by dropping generic words like "review", "overview", "investigation", "request", "question", or "this project".

The title should read naturally as a short phrase, not as a stack of nouns. Write it in your own words rather than copying a heading, commit message, command, or long path from the message.

Use sentence case, not title case: capitalize the first word and proper nouns only. Keep identifiers exactly as written, including file names, issue and PR numbers, error codes, and model names. Use the same language as the message.

The message is something to name, not something to answer or carry out. Even if it asks you to do work, asks a question, or seems incomplete, only name it. For greetings or small talk, use a short label like "Greeting".

Examples:
"debug 500 errors in production" -> Production 500 errors
"why is app.js failing" -> app.js failure
"what does the retry logic in fetch.ts do" -> fetch.ts retry logic
"@src/credential.ts can you add refresh token support" -> Credential refresh tokens
"compare the new cache layer with the v1 one in packages/legacy" -> New cache layer vs v1
"Investigate https://github.com/acme/app/issues/412, don't make edits" -> Issue #412
"hey" -> Greeting"#;

const PROMPT_SUMMARY: &str = r#"Summarize what was done in this conversation. Write like a pull request description.

Rules:
- 2-3 sentences max
- Describe the changes made, not the process
- Do not mention running tests, builds, or other validation steps
- Do not explain what the user asked for
- Write in first person (I added..., I fixed...)
- Never ask questions or add new questions
- If the conversation ends with an unanswered question to the user, preserve that exact question
- If the conversation ends with an imperative statement or request to the user (e.g. "Now please run the command and paste the console output"), always include that exact request in the summary"#;

#[derive(Debug, Clone)]
pub(crate) struct AgentState {
    pub id: String,
    pub name: String,
    pub model: Option<Value>,
    pub system: Option<String>,
    pub description: Option<String>,
    pub mode: String,
    pub hidden: bool,
    pub color: Option<String>,
    pub steps: Option<u64>,
    headers: Map<String, Value>,
    body: Map<String, Value>,
    pub permissions: Vec<PermissionRule>,
}

impl AgentState {
    fn new(id: &str, name: &str, mode: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            model: None,
            system: None,
            description: None,
            mode: mode.to_owned(),
            hidden: false,
            color: None,
            steps: None,
            headers: Map::new(),
            body: Map::new(),
            permissions: default_rules(),
        }
    }

    fn custom(id: &str) -> Self {
        Self::new(id, id, "primary")
    }

    #[must_use]
    pub(crate) fn is_selectable(&self) -> bool {
        self.mode != "subagent" && !self.hidden
    }

    pub(crate) fn push_document_permissions(&mut self, rules: &[PermissionRule], home: &Path) {
        let mut rules = rules.to_vec();
        expand_home(&mut rules, home);
        self.permissions.extend(rules);
    }

    /// Apply one normalized `agents.<id>` entry from one document.
    pub(crate) fn apply(&mut self, item: &Map<String, Value>, home: &Path) {
        if let Some(model) = item.get("model") {
            self.model = Some(model.clone());
        }
        if let Some(request) = item.get("request").and_then(Value::as_object) {
            if let Some(headers) = request.get("headers").and_then(Value::as_object) {
                for (key, value) in headers {
                    self.headers.insert(key.clone(), value.clone());
                }
            }
            if let Some(body) = request.get("body").and_then(Value::as_object) {
                for (key, value) in body {
                    self.body.insert(key.clone(), value.clone());
                }
            }
        }
        if let Some(value) = item.get("system").and_then(Value::as_str) {
            self.system = Some(value.to_owned());
        }
        if let Some(value) = item.get("description").and_then(Value::as_str) {
            self.description = Some(value.to_owned());
        }
        if let Some(value) = item.get("mode").and_then(Value::as_str) {
            value.clone_into(&mut self.mode);
        }
        if let Some(value) = item.get("hidden").and_then(Value::as_bool) {
            self.hidden = value;
        }
        if let Some(value) = item.get("color").and_then(Value::as_str) {
            self.color = Some(value.to_owned());
        }
        if let Some(value) = item.get("steps").and_then(Value::as_u64) {
            self.steps = Some(value);
        }
        if let Some(value) = item.get("permissions").and_then(Value::as_array) {
            let mut rules = rules_from_values(value);
            expand_home(&mut rules, home);
            self.permissions.extend(rules);
        }
    }

    #[must_use]
    pub(crate) fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".to_owned(), Value::String(self.id.clone()));
        map.insert("name".to_owned(), Value::String(self.name.clone()));
        if let Some(model) = &self.model {
            map.insert("model".to_owned(), agent_model_ref(model));
        }
        map.insert(
            "request".to_owned(),
            serde_json::json!({
                "settings": {},
                "headers": Value::Object(self.headers.clone()),
                "body": Value::Object(self.body.clone()),
            }),
        );
        if let Some(system) = &self.system {
            map.insert("system".to_owned(), Value::String(system.clone()));
        }
        if let Some(description) = &self.description {
            map.insert("description".to_owned(), Value::String(description.clone()));
        }
        map.insert("mode".to_owned(), Value::String(self.mode.clone()));
        map.insert("hidden".to_owned(), Value::Bool(self.hidden));
        if let Some(color) = &self.color {
            map.insert("color".to_owned(), Value::String(color.clone()));
        }
        if let Some(steps) = self.steps {
            map.insert("steps".to_owned(), Value::from(steps));
        }
        map.insert(
            "permissions".to_owned(),
            Value::Array(
                self.permissions
                    .iter()
                    .map(|rule| {
                        serde_json::json!({
                            "action": rule.action,
                            "resource": rule.resource,
                            "effect": rule.effect.as_str(),
                        })
                    })
                    .collect(),
            ),
        );
        Value::Object(map)
    }
}

/// Convert a config model selection `{providerID, model, variant?}` into the
/// wire `Agent.Info` model reference `{id, providerID, variant?}`.
fn agent_model_ref(model: &Value) -> Value {
    let Some(object) = model.as_object() else {
        return model.clone();
    };
    let mut map = Map::new();
    if let Some(provider) = object.get("providerID") {
        map.insert("providerID".to_owned(), provider.clone());
    }
    if let Some(id) = object.get("model") {
        map.insert("id".to_owned(), id.clone());
    }
    if let Some(variant) = object.get("variant") {
        map.insert("variant".to_owned(), variant.clone());
    }
    Value::Object(map)
}

fn rules_from_values(values: &[Value]) -> Vec<PermissionRule> {
    values
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let action = object.get("action")?.as_str()?;
            let resource = object.get("resource")?.as_str()?;
            let effect = object.get("effect")?.as_str().and_then(Effect::parse)?;
            Some(PermissionRule::new(action, resource, effect))
        })
        .collect()
}

fn allow(action: &str, resource: &str) -> PermissionRule {
    PermissionRule::new(action, resource, Effect::Allow)
}

fn deny(action: &str, resource: &str) -> PermissionRule {
    PermissionRule::new(action, resource, Effect::Deny)
}

fn ask(action: &str, resource: &str) -> PermissionRule {
    PermissionRule::new(action, resource, Effect::Ask)
}

/// Runtime-level alias for a stored or legacy agent id.
///
/// Trims the input and matches it case-insensitively against the pre-rename
/// upstream ids: `build` -> `engineer`, `plan` -> `architect`, `reviewer` ->
/// `review`. Anything else is returned trimmed but otherwise untouched, so a
/// custom agent id is never rewritten. Session rows, spawn targets and the
/// observer resolve stored ids through this function.
#[must_use]
pub fn normalize_agent_id(id: &str) -> &str {
    let trimmed = id.trim();
    if trimmed.eq_ignore_ascii_case("build") {
        "engineer"
    } else if trimmed.eq_ignore_ascii_case("plan") {
        "architect"
    } else if trimmed.eq_ignore_ascii_case("reviewer") {
        "review"
    } else {
        trimmed
    }
}

/// The ten built-in agents in their canonical order.
///
/// `engineer`, `architect`, `review`, `general`, `explore`, `ui`, `docs`, then
/// the hidden system agents `compaction`, `title`, `summary`.
#[must_use]
pub(crate) fn builtins(home: &Path) -> Vec<AgentState> {
    // Preserve the historical `~/.opencode` expansion for the home-scoped
    // resources below; the parameter stays part of the signature so callers do
    // not change.
    let _ = home;

    let mut engineer = AgentState::new("engineer", "Engineer", "primary");
    engineer.description = Some(
        "The default implementation agent. Plans, implements, tests, and verifies changes across the workspace.".to_owned(),
    );
    engineer.system = Some(PROMPT_ENGINEER.to_owned());
    engineer.permissions.push(allow("question", "*"));

    // Docs-only: deny writes everywhere, then allow them back under `docs/`.
    // `evaluate_rules` is last-match-wins, so the order below is what grants
    // the architect its one writable tree.
    let mut architect = AgentState::new("architect", "Architect", "primary");
    architect.description = Some(
        "Docs-first design agent. Writes plans and design notes under docs/ and does not edit code.".to_owned(),
    );
    architect.system = Some(PROMPT_ARCHITECT.to_owned());
    architect.permissions.push(allow("question", "*"));
    architect.permissions.push(deny("edit", "*"));
    architect.permissions.push(deny("write", "*"));
    architect.permissions.push(deny("patch", "*"));
    architect.permissions.push(allow("edit", "docs/**"));
    architect.permissions.push(allow("write", "docs/**"));
    architect.permissions.push(allow("patch", "docs/**"));

    let mut review = AgentState::new("review", "Review", "primary");
    review.description = Some(
        "Read-only review agent. Inspects diffs, runs verification gates, and checks invariants without editing files.".to_owned(),
    );
    review.system = Some(PROMPT_REVIEW.to_owned());
    review.permissions.push(allow("question", "*"));
    review.permissions.push(deny("edit", "*"));
    review.permissions.push(deny("write", "*"));
    review.permissions.push(deny("patch", "*"));

    let mut general = AgentState::new("general", "General", "subagent");
    general.description = Some(
        "General-purpose agent for researching complex questions and executing multi-step tasks. Use this agent to execute multiple units of work in parallel.".to_owned(),
    );
    general.permissions.push(deny("question", "*"));
    // No `subagent` deny: nesting is governed by `subagents.max_depth`,
    // `subagents.allow_spawn` and the per-agent `subagents.agents.<id>`
    // overrides, which the runtime/policy enforce at spawn time.

    let mut explore = AgentState::new("explore", "Explore", "subagent");
    explore.description = Some(
        "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.".to_owned(),
    );
    explore.system = Some(PROMPT_EXPLORE.to_owned());
    explore.permissions.push(deny("*", "*"));
    explore.permissions.push(allow("shell", "*"));
    explore.permissions.push(allow("grep", "*"));
    explore.permissions.push(allow("glob", "*"));
    explore.permissions.push(allow("webfetch", "*"));
    explore.permissions.push(allow("websearch", "*"));
    explore.permissions.push(allow("read", "*"));
    // `code_map` is read-only structural search; it belongs in the explore
    // toolset. Its permission action is the tool name itself (the runtime
    // maps only write/edit/patch/symbol_edit to `edit`, except `symbol_edit`
    // preview which maps to `read`).
    explore.permissions.push(allow("code_map", "*"));
    // `key_health` is read-only credential introspection; every agent may call
    // it. Its action is likewise the tool name, so `deny("*","*")` above hides
    // it unless it is allowed explicitly here.
    explore.permissions.push(allow("key_health", "*"));
    // Boxes are session-tree working state; explore may read them via
    // `box_list`/`box_read`/`box_search`, and `skill` allows read-only skill loading.
    explore.permissions.push(allow("box:read", "*"));
    explore.permissions.push(allow("skill", "*"));
    explore.permissions.push(ask("read", "*.env"));
    explore.permissions.push(ask("read", "*.env.*"));
    explore.permissions.push(allow("read", "*.env.example"));
    explore.permissions.push(deny("subagent", "*"));
    explore.permissions.push(allow("external_directory", "*"));

    let mut ui = AgentState::new("ui", "UI", "subagent");
    ui.description = Some(
        "Frontend UI agent. Owns ui/** and web/**, keeping pickers and live views synchronized with server state."
            .to_owned(),
    );
    ui.system = Some(PROMPT_UI.to_owned());
    ui.permissions.push(allow("question", "*"));
    ui.permissions.push(deny("edit", "*"));
    ui.permissions.push(deny("write", "*"));
    ui.permissions.push(deny("patch", "*"));
    ui.permissions.push(allow("edit", "ui/**"));
    ui.permissions.push(allow("write", "ui/**"));
    ui.permissions.push(allow("patch", "ui/**"));
    ui.permissions.push(allow("edit", "web/**"));
    ui.permissions.push(allow("write", "web/**"));
    ui.permissions.push(allow("patch", "web/**"));

    let mut docs = AgentState::new("docs", "Docs", "subagent");
    docs.description = Some(
        "Documentation owner. Writes docs/**, README.md and AGENTS.md and never edits code."
            .to_owned(),
    );
    docs.system = Some(PROMPT_DOCS.to_owned());
    docs.permissions.push(allow("question", "*"));
    docs.permissions.push(deny("edit", "*"));
    docs.permissions.push(deny("write", "*"));
    docs.permissions.push(deny("patch", "*"));
    for resource in ["docs/**", "README.md", "AGENTS.md"] {
        docs.permissions.push(allow("edit", resource));
        docs.permissions.push(allow("write", resource));
        docs.permissions.push(allow("patch", resource));
    }

    let mut compaction = AgentState::new("compaction", "Compaction", "primary");
    compaction.hidden = true;
    compaction.permissions.push(deny("*", "*"));

    let mut title = AgentState::new("title", "Title", "primary");
    title.hidden = true;
    title.system = Some(PROMPT_TITLE.to_owned());
    title.permissions.push(deny("*", "*"));

    let mut summary = AgentState::new("summary", "Summary", "primary");
    summary.hidden = true;
    summary.system = Some(PROMPT_SUMMARY.to_owned());
    summary.permissions.push(deny("*", "*"));

    vec![
        engineer, architect, review, general, explore, ui, docs, compaction, title, summary,
    ]
}

/// Create or update the agent named by a document entry.
pub(crate) fn resolve<'a>(
    agents: &'a mut Vec<AgentState>,
    id: &str,
    home: &Path,
    document_permissions: &[PermissionRule],
) -> &'a mut AgentState {
    let index = if let Some(index) = agents.iter().position(|agent| agent.id == id) {
        index
    } else {
        let mut agent = AgentState::custom(id);
        agent.push_document_permissions(document_permissions, home);
        agents.push(agent);
        agents.len() - 1
    };
    &mut agents[index]
}

/// Apply markdown-defined agents after every JSON document, so a file wins
/// over configuration while built-ins keep their defaults when a field is not
/// overridden.
pub(crate) fn apply_markdown(
    agents: &mut Vec<AgentState>,
    entries: &[MarkdownAgent],
    home: &Path,
    document_permissions: &[PermissionRule],
    diagnostics: &mut Vec<Diagnostic>,
) {
    for entry in entries {
        if entry
            .frontmatter
            .get("disabled")
            .and_then(crate::markdown::coerce_bool)
            == Some(true)
        {
            agents.retain(|agent| agent.id != entry.id);
            continue;
        }
        let existed = agents.iter().any(|agent| agent.id == entry.id);
        let source = entry.path.to_string_lossy().into_owned();
        let state = resolve(agents, &entry.id, home, document_permissions);
        if entry.frontmatter_invalid {
            // Fail closed: a file that declared permissions it could not
            // parse must not inherit the permissive defaults. Regression:
            // tests/agent_frontmatter.rs::unparsable_frontmatter_denies_everything
            state.permissions = vec![deny("*", "*")];
        }
        if !existed {
            "all".clone_into(&mut state.mode);
        }

        let mut item = Map::new();
        item.insert("system".to_owned(), Value::String(entry.body.clone()));
        if let Some(value) = entry.frontmatter.get("description").and_then(Value::as_str) {
            item.insert("description".to_owned(), Value::String(value.to_owned()));
        }
        if let Some(value) = entry.frontmatter.get("color").and_then(Value::as_str) {
            item.insert("color".to_owned(), Value::String(value.to_owned()));
        }
        if let Some(value) = entry
            .frontmatter
            .get("hidden")
            .and_then(crate::markdown::coerce_bool)
        {
            item.insert("hidden".to_owned(), Value::Bool(value));
        }
        if let Some(value) = entry.frontmatter.get("steps").and_then(Value::as_u64) {
            item.insert("steps".to_owned(), Value::from(value));
        }
        if let Some(mode) = entry.forced_mode.clone().or_else(|| {
            entry
                .frontmatter
                .get("mode")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }) {
            item.insert("mode".to_owned(), Value::String(mode));
        }
        if let Some(value) = entry.frontmatter.get("model") {
            match crate::normalize::model_selection(value) {
                Some(selection) => {
                    item.insert("model".to_owned(), selection);
                }
                None => diagnostics.push(Diagnostic::new(
                    Some(&source),
                    DiagnosticKind::Invalid,
                    "$.model",
                    "invalid model reference in agent frontmatter; ignored",
                )),
            }
        }
        let mut body = Map::new();
        for key in ["temperature", "top_p"] {
            if let Some(value) = entry.frontmatter.get(key).filter(|value| value.is_number()) {
                body.insert(key.to_owned(), value.clone());
            }
        }
        if !body.is_empty() {
            item.insert(
                "request".to_owned(),
                serde_json::json!({ "body": Value::Object(body) }),
            );
        }
        state.apply(&item, home);
    }
}
