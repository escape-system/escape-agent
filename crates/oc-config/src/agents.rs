//! Built-in agents and application of document-level agent configuration.
//!
//! Permissions compose as `built-in defaults ++ document permissions ++
//! per-agent permissions`, so a later, more specific rule wins (docs/14).

use std::path::Path;

use serde_json::{Map, Value};

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::markdown_agents::MarkdownAgent;
use crate::permissions::{Effect, PermissionRule, default_rules, expand_home};

const PROMPT_EXPLORE: &str =
    "You are a file search specialist. You excel at thoroughly navigating and exploring codebases.

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

/// The seven built-in agents in their canonical order.
#[must_use]
pub(crate) fn builtins(home: &Path) -> Vec<AgentState> {
    let mut build = AgentState::new("build", "Build", "primary");
    build.description =
        Some("The default agent. Executes tools based on configured permissions.".to_owned());
    build.permissions.push(allow("question", "*"));

    let plan_directory = if home.as_os_str().is_empty() {
        "~/.opencode/plan/*".to_owned()
    } else {
        home.join(".opencode")
            .join("plan")
            .join("*")
            .to_string_lossy()
            .into_owned()
    };
    let mut plan = AgentState::new("plan", "Plan", "primary");
    plan.description = Some(
        "Read-only agent for exploring the codebase and planning work before implementation."
            .to_owned(),
    );
    plan.permissions.push(allow("question", "*"));
    plan.permissions.push(deny("edit", "*"));
    plan.permissions.push(allow("edit", &plan_directory));

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
    // Boxes are the shared working context of the session tree; explore may
    // read them but never write them (`docs/25-context-boxes.md`).
    explore.permissions.push(allow("box:read", "*"));
    explore.permissions.push(ask("read", "*.env"));
    explore.permissions.push(ask("read", "*.env.*"));
    explore.permissions.push(allow("read", "*.env.example"));
    explore.permissions.push(deny("subagent", "*"));
    explore.permissions.push(allow("external_directory", "*"));

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

    vec![build, plan, general, explore, compaction, title, summary]
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
