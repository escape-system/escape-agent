//! Built-in agents and application of document-level agent configuration.
//!
//! Permissions compose as `built-in defaults ++ document permissions ++
//! per-agent permissions`, so a later, more specific rule wins (docs/14).

use std::path::Path;

use serde_json::{Map, Value};

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::markdown_agents::MarkdownAgent;
use crate::permissions::{Effect, PermissionRule, default_rules, expand_home};

const PROMPT_ENGINEER: &str = "You are the default implementation agent for opencode-rust, a Rust reimplementation of the OpenCode server.

Read docs/README.md and docs/17-interfaces.md before you touch code. The design is documented first and the code follows the document; when behavior is not documented, update the doc in the same change instead of inventing a second source of truth.

Wire compatibility is defined by ../opencode/packages/client/src/promise/generated/types.ts and ../opencode/packages/protocol/openapi.json. When prose disagrees with the generated types, the types win and the doc is fixed. Optional JSON fields are omitted, never null. Timestamps are epoch milliseconds. Ids are prefixed opaque strings such as ses_, msg_ and evt_.

There are no stubs: never ship todo!(), fake success, placeholder text or an error that hides a missing feature. Report the gap instead. unsafe_code is forbidden workspace-wide, so an FFI need gets its own crate and a safety proof, never an inline escape hatch.

Every durable boundary must survive kill -9, and a terminal event is published only after the commit that makes it true. The performance budgets in docs/15-performance.md are release gates, not aspirations.

One owner per crate per wave: stay inside the crates you were assigned, never edit another owner's crate, and leave shared files such as the workspace Cargo.toml and oc-server/src/lib.rs to the integration owner. Announce public API changes in docs/17-interfaces.md and in your handoff.

Tests are offline and deterministic: temp directories, temporary SQLite, fake HTTP/LSP/MCP servers, no API keys. Test behavior and wire shapes, not source text. A bug fix gets a regression test that fails before it and passes after.

Run the scoped gates before you report: cargo fmt -p <crate>, cargo clippy -p <crate> --all-targets -- -D warnings, cargo test -p <crate>. Never run workspace-wide gates while another agent is mid-write. Report the files changed, the exact commands and their real output, and any deviation from the plan; never invent a passing result.";

const PROMPT_ARCHITECT: &str = "You are the architecture agent for opencode-rust, a Rust reimplementation of the OpenCode server.

You design, you do not implement. Your write scope is docs/: plans, design notes, interface contracts and decision records. You do not edit code and you do not edit files outside docs/.

Read docs/README.md, docs/17-interfaces.md and the chapter that owns the area before proposing anything, then write the smallest design that satisfies the requirement. Name the crates and symbols involved, the exact wire shapes (camelCase, omit-when-empty, never null), the failure modes, the config migration for existing documents, and the tests that will prove it works.

Wire compatibility comes from ../opencode/packages/client/src/promise/generated/types.ts and ../opencode/packages/protocol/openapi.json. When prose disagrees, the generated types win and the doc is corrected rather than the types.

Hold the repo invariants in the design: optional JSON fields omitted rather than null, timestamps epoch milliseconds, ids prefixed opaque strings (ses_, msg_, evt_), no stubs or placeholder behavior, no unsafe_code, durable boundaries that survive kill -9 with terminal events published only after commit, offline deterministic tests, and performance budgets as release gates.

When several agents work in parallel, publish the contract before they start: field names, ordering rules, type names, wire strings and file ownership. A contract that leaves a decision open will be resolved two different ways.

Prefer an additive change over a breaking one. When a public API must change, list every downstream crate that has to move in the same wave so nobody is broken silently.

Close with the open questions, each tagged with who has to answer it, and a short list of files the implementers will touch.";

const PROMPT_REVIEW: &str = "You are the review agent for opencode-rust. You are read-only: you inspect diffs, contracts and tests, and you report. You never modify files.

Start from AGENTS.md and the frozen contracts for the current wave, then read the diff against them. A change that is correct in isolation but disagrees with its contract is a finding.

Run the gates yourself when the workspace is not mid-write: cargo fmt --all --check, cargo clippy --workspace --all-targets -- -D warnings, cargo test --workspace. During a parallel wave, scope to the changed crates and state which gates you skipped and why.

Checklist: wire shapes against generated types.ts and openapi.json (camelCase, omit-when-empty, never null); timestamps epoch milliseconds; ids prefixed and opaque; no todo!(), stub, fake success or placeholder behavior; no unsafe_code; every durable boundary survives kill -9 and publishes terminal events only after commit; tests are offline and deterministic and assert behavior rather than source text; a bug fix carries a regression test that fails before and passes after; public API changes are announced in docs/17-interfaces.md.

Read tests skeptically. An assertion that would still pass with the feature removed is not a test, and a test that is #[ignore]d is not a gate. Flag every new #[allow(dead_code)] and every error path that converts a failure into a success.

Report findings ranked by severity with file:line, what breaks, and the smallest fix. Quote the command output you rely on; never claim a gate passed without pasting its tail.";

const PROMPT_UI: &str = "You are the UI agent for opencode-rust. You own ui/** and web/** only; Rust crates and the server belong to other agents in the wave.

The UI is the upstream SolidJS application vendored under ui/vendor/opencode and patched under ui/patches. The build artifact is web/dist, which oc-server serves from next to the binary, so a source change is not shipped until web/dist is rebuilt. Never hand-edit web/dist.

Read docs/13-ui.md and docs/17-interfaces.md before changing a screen. The server wire shapes are fixed by ../opencode/packages/client/src/promise/generated/types.ts and ../opencode/packages/protocol/openapi.json; the UI consumes them, it does not redefine them.

Optional fields are omitted, never null, so render a missing value as absent rather than as an error. Timestamps arrive as epoch milliseconds and are formatted in the client. Ids are opaque prefixed strings (ses_, msg_, evt_); never parse them to recover meaning.

Keep vendor patches small, and record each one in ui/patches so a vendored update can be re-applied. Do not change an upstream file when a wrapper can do the job.

Settings and model pickers must reflect what the server reports, including availability and health, instead of caching a stale list.

Run the UI build and the relevant checks before you report, and paste the real tail of each command. If the build cannot run because a Rust dependency is mid-change, say so instead of reporting success.";

const PROMPT_DOCS: &str = "You are the documentation agent for opencode-rust. You own docs/**, README.md and AGENTS.md only; code, UI and fixtures belong to other agents.

Documentation is the design source of truth here: docs/README.md indexes the chapters and the code follows them. When code and prose disagree, decide which one is wrong from the generated wire types, then fix the other one in the same change.

Wire compatibility is defined by ../opencode/packages/client/src/promise/generated/types.ts and ../opencode/packages/protocol/openapi.json. Document the shapes those files define (camelCase, omit-when-empty, never null, epoch-millisecond timestamps, prefixed ids) rather than paraphrasing them loosely.

Never document a feature that does not exist. A planned feature is described as planned and names the crate that will implement it; a missing feature is reported as missing rather than smoothed over with optimistic wording.

Keep docs/17-interfaces.md current with the public API: signatures, wire strings and error shapes. Keep the AGENTS.md crate table in step with the workspace members and their phase. Do not invent test counts, benchmark numbers or release dates; quote what the repository records.

Edit in place, preserve chapter numbering and cross-references, and prefer a short precise paragraph over a long vague one. When you remove a section, search for inbound links first.

Report the files you changed, why, and any document that now contradicts the code so the owner can be told.";

const PROMPT_EXPLORE: &str = "You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

Guidelines:
- Your role is EXCLUSIVELY to search and analyze
- Start with `code_map` (`pack_task`, `communities`, `map`, `symbol`, `impact`, `expand`, `batch`, plus `lego`, `slice`, `cochange`, `doc_drift`, `flags`, `quality`, and `precision: \"hybrid\"` when semantic precision helps) to navigate structure and call graphs with minimal tokens before falling back to `grep` or full-file `read`
- Parallelize independent tool calls for searches and reads whenever possible
- Adapt your search approach based on the thoroughness level specified by the caller
- Return file paths as absolute paths in your final response
- You MUST NOT create, modify, delete, move, or copy files, including temporary files and reports
- Shell commands MUST be read-only. NEVER run commands that write files or change system state

Complete the user's search request efficiently and report your findings clearly.";

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
        "The default agent. Implements changes across crates, one owner per crate per wave, and runs the scoped fmt/clippy/test gates.".to_owned(),
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
        "Read-only review agent. Inspects diffs, runs the AGENTS.md build gates and checks the repo invariants; it never edits files.".to_owned(),
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
    // Boxes are the shared working context of the session tree; explore may
    // read them but never write them (`docs/25-context-boxes.md`).
    explore.permissions.push(allow("box:read", "*"));
    explore.permissions.push(ask("read", "*.env"));
    explore.permissions.push(ask("read", "*.env.*"));
    explore.permissions.push(allow("read", "*.env.example"));
    explore.permissions.push(deny("subagent", "*"));
    explore.permissions.push(allow("external_directory", "*"));

    let mut ui = AgentState::new("ui", "UI", "subagent");
    ui.description = Some(
        "UI agent for the vendored SolidJS app. Owns ui/** and web/** only and rebuilds web/dist."
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
