//! Configuration service: discovery, JSONC parsing, V1 normalization, merging,
//! built-in agents and permission evaluation.
//!
//! Loading never fails because of a single document: unreadable or malformed
//! sources are skipped and recorded in [`ConfigService::diagnostics`]. There is
//! no network access and no write to user configuration.

mod agents;
mod commands;
mod diagnostic;
mod discovery;
mod instructions;
mod jsonc;
pub(crate) mod markdown;
mod markdown_agents;
mod merge;
mod normalize;
mod permissions;
mod scan;
mod skills;
mod substitute;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

pub use commands::CommandInfo;
pub use diagnostic::{Diagnostic, DiagnosticKind};
use discovery::{Discovery, Env};
pub use instructions::InstructionFile;
pub(crate) use jsonc::parse_jsonc;
/// Frontmatter reader shared with callers that validate a skill file before
/// writing it (`skill_lint`).
pub use markdown::{Parsed as MarkdownDocument, parse as parse_markdown};
use merge::merge_documents;
pub use permissions::{Effect, PermissionRule, evaluate_rules, wildcard_match};
pub use skills::{SkillInfo, SkillSource};

use agents::AgentState;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration load failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub directory: PathBuf,
    /// Environment overrides layered over the process environment. Tests use
    /// this instead of mutating global process state.
    pub env: HashMap<String, String>,
}

impl LoadOptions {
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            env: HashMap::new(),
        }
    }

    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Document,
    Directory,
}

/// One entry of `GET /api/config`, lowest to highest priority.
#[derive(Debug, Clone)]
pub struct ConfigEntry {
    pub kind: EntryKind,
    /// Filesystem path; absent for inline content.
    pub path: Option<String>,
    /// Normalized document for [`EntryKind::Document`]. Directory entries use
    /// `Value::Null`; serialization emits only `type` and `path` for them.
    pub info: Value,
}

impl ConfigEntry {
    #[must_use]
    pub fn document(path: Option<String>, info: Value) -> Self {
        Self {
            kind: EntryKind::Document,
            path,
            info,
        }
    }

    #[must_use]
    pub fn directory(path: impl Into<String>) -> Self {
        Self {
            kind: EntryKind::Directory,
            path: Some(path.into()),
            info: Value::Null,
        }
    }

    /// Wire representation: `{type:"document",path?,info}` or
    /// `{type:"directory",path}`.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        match self.kind {
            EntryKind::Document => {
                map.insert("type".to_owned(), Value::String("document".to_owned()));
                if let Some(path) = &self.path {
                    map.insert("path".to_owned(), Value::String(path.clone()));
                }
                map.insert("info".to_owned(), self.info.clone());
            }
            EntryKind::Directory => {
                map.insert("type".to_owned(), Value::String("directory".to_owned()));
                if let Some(path) = &self.path {
                    map.insert("path".to_owned(), Value::String(path.clone()));
                }
            }
        }
        Value::Object(map)
    }
}

impl Serialize for ConfigEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_value().serialize(serializer)
    }
}

#[derive(Debug)]
pub struct ConfigService {
    entries: Vec<ConfigEntry>,
    documents: Vec<Value>,
    discovery: Discovery,
    merged: Value,
    agents: Vec<AgentState>,
    default_agent: String,
    permissions: Vec<PermissionRule>,
    diagnostics: Vec<Diagnostic>,
    skills_roots: Vec<PathBuf>,
    skills: Vec<SkillInfo>,
    commands: Vec<CommandInfo>,
    instructions: Vec<InstructionFile>,
    mcp_servers: Value,
    shell: String,
    model: Option<String>,
}

impl ConfigService {
    pub fn load(options: LoadOptions) -> Result<Self, ConfigError> {
        let env = Env::new(&options.env);
        let directory = if options.directory.as_os_str().is_empty() {
            std::env::current_dir()?
        } else {
            options.directory
        };
        let discovery = discovery::discover(&directory, &env);

        let mut loader = Loader::new(&discovery, &env);
        loader.load_all();

        // End the loader's borrow of `discovery` before storing it.
        let Loader {
            entries,
            documents,
            mut diagnostics,
            ..
        } = loader;

        let mut permissions = global_permissions(&documents);
        permissions::expand_home(&mut permissions, &discovery.home);
        let (config_roots, extra_skill_roots) =
            collect_skill_roots(&entries, &documents, &discovery, &mut diagnostics);
        let skills = skills::discover(
            &config_roots,
            &extra_skill_roots,
            &discovery,
            &mut diagnostics,
        );
        let commands = commands::discover(&config_roots, &mut diagnostics);
        let instructions = instructions::discover(&discovery.location, &mut diagnostics);
        let markdown_agents =
            markdown_agents::discover(&config_roots, &discovery, &mut diagnostics);

        let merged = merge_documents(&documents);
        let (agents, default_agent) = build_agents(
            &documents,
            &permissions,
            &discovery.home,
            &markdown_agents,
            &mut diagnostics,
        );
        let skills_roots = visible_skill_roots(&config_roots, &extra_skill_roots);
        let mcp_servers = merged
            .get("mcp")
            .and_then(|mcp| mcp.get("servers"))
            .filter(|servers| servers.is_object())
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let shell = merged
            .get("shell")
            .and_then(Value::as_str)
            .map_or_else(default_shell, str::to_owned);
        let model = merged.get("model").and_then(normalize::model_string);

        Ok(Self {
            entries,
            documents,
            discovery,
            merged,
            agents,
            default_agent,
            permissions,
            diagnostics,
            skills_roots,
            skills,
            commands,
            instructions,
            mcp_servers,
            shell,
            model,
        })
    }

    /// Documents and directories for `GET /api/config`, lowest priority first.
    #[must_use]
    pub fn entries(&self) -> &[ConfigEntry] {
        &self.entries
    }

    /// Last-wins scalar merge with per-document folding of map-like keys.
    #[must_use]
    pub fn merged(&self) -> &Value {
        &self.merged
    }

    /// Built-in and configured agents as wire `Agent.Info` objects, with the
    /// selected default agent first.
    #[must_use]
    pub fn agents(&self) -> Vec<Value> {
        let mut values = Vec::with_capacity(self.agents.len());
        if let Some(agent) = self
            .agents
            .iter()
            .find(|agent| agent.id == self.default_agent)
        {
            values.push(agent.to_value());
        }
        for agent in &self.agents {
            if agent.id != self.default_agent {
                values.push(agent.to_value());
            }
        }
        values
    }

    #[must_use]
    pub fn agent(&self, id: &str) -> Option<Value> {
        self.agents
            .iter()
            .find(|agent| agent.id == id)
            .map(AgentState::to_value)
    }

    /// Document-level permission rules in load order.
    #[must_use]
    pub fn permissions(&self) -> &[PermissionRule] {
        &self.permissions
    }

    /// Evaluate an action against the agent's composed rules.
    ///
    /// Agent rules already include the document-level permissions (pushed
    /// before per-agent rules, so the agent wins). Without a known agent, only
    /// document rules apply. No match means `ask`.
    #[must_use]
    pub fn evaluate(&self, agent: Option<&str>, action: &str, resource: &str) -> Effect {
        if let Some(agent) = agent
            && let Some(state) = self.agents.iter().find(|state| state.id == agent)
        {
            return evaluate_rules(state.permissions.iter(), action, resource);
        }
        evaluate_rules(self.permissions.iter(), action, resource)
    }

    /// Configuration roots that contribute `skill/` and `skills/`, in load
    /// order. Local `skills[]` entries are resolved; URL entries are skipped
    /// because loading never touches the network.
    #[must_use]
    pub fn skills_roots(&self) -> Vec<PathBuf> {
        self.skills_roots.clone()
    }

    /// Discovered skills, sorted by id. Later roots and files win on id
    /// collisions; URL bundles are skipped (no network) with a diagnostic.
    #[must_use]
    pub fn skills(&self) -> &[SkillInfo] {
        &self.skills
    }

    #[must_use]
    pub fn skill(&self, id: &str) -> Option<&SkillInfo> {
        self.skills.iter().find(|skill| skill.id == id)
    }

    /// Re-run skill discovery over the same roots and return the sorted
    /// result. Use after writing a `SKILL.md`; the service itself keeps its
    /// load-time snapshot.
    #[must_use]
    pub fn rescan_skills(&self) -> Vec<SkillInfo> {
        let mut diagnostics = Vec::new();
        let (config_roots, extra_roots) = collect_skill_roots(
            &self.entries,
            &self.documents,
            &self.discovery,
            &mut diagnostics,
        );
        skills::discover(
            &config_roots,
            &extra_roots,
            &self.discovery,
            &mut diagnostics,
        )
    }

    /// The global configuration directory (`$XDG_CONFIG_HOME/opencode` or
    /// `~/.config/opencode`) discovered at load time.
    #[must_use]
    pub fn global_config_dir(&self) -> Option<PathBuf> {
        (!self.discovery.global.as_os_str().is_empty()).then(|| self.discovery.global.clone())
    }

    /// Markdown slash commands, sorted by name.
    #[must_use]
    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    #[must_use]
    pub fn command(&self, name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|command| command.name == name)
    }

    /// `AGENTS.md`/`CLAUDE.md`/`.opencode/AGENTS.md` from the location upward,
    /// outermost first.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionFile] {
        &self.instructions
    }

    /// Merged `mcp.servers` object (empty when unconfigured).
    #[must_use]
    pub fn mcp_servers(&self) -> Value {
        self.mcp_servers.clone()
    }

    /// Configured shell, or the platform default.
    #[must_use]
    pub fn shell(&self) -> String {
        self.shell.clone()
    }

    /// Default model reference as `provider/model[#variant]`.
    #[must_use]
    pub fn model(&self) -> Option<String> {
        self.model.clone()
    }

    /// Selected default primary agent.
    #[must_use]
    pub fn default_agent(&self) -> &str {
        &self.default_agent
    }

    /// Warnings collected while loading: dropped legacy keys, malformed
    /// documents, missing `{file:...}` references.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Normalized documents in load order.
    #[must_use]
    pub fn documents(&self) -> &[Value] {
        &self.documents
    }
}

struct Loader<'a> {
    discovery: &'a Discovery,
    env: &'a Env,
    entries: Vec<ConfigEntry>,
    documents: Vec<Value>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Loader<'a> {
    fn new(discovery: &'a Discovery, env: &'a Env) -> Self {
        Self {
            discovery,
            env,
            entries: Vec::new(),
            documents: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn load_all(&mut self) {
        // 1. Global directory (always represented, files when present).
        let global = self.discovery.global.clone();
        self.load_directory(&global);

        // 2. Explicit `OPENCODE_CONFIG` file.
        if let Some(path) = self.discovery.explicit.clone()
            && path.is_file()
        {
            self.load_file(&path);
        }

        // 3. Ancestor direct files, outermost first.
        let direct = self.discovery.direct.clone();
        for path in &direct {
            self.load_file(path);
        }

        // 4. Project `.opencode` directories, outermost first.
        let project = self.discovery.project.clone();
        for directory in &project {
            self.load_directory(directory);
        }

        // 5. Inline `OPENCODE_CONFIG_CONTENT`.
        if let Some(content) = self.discovery.content.clone()
            && !content.trim().is_empty()
        {
            let location = self.discovery.location.clone();
            self.parse_and_push(Some("OPENCODE_CONFIG_CONTENT"), &content, &location, None);
        }

        // 6. Managed system directory, highest priority.
        if self.discovery.system_present {
            let system = self.discovery.system.clone();
            self.load_directory(&system);
        }
    }

    fn load_directory(&mut self, directory: &Path) {
        for name in discovery::NAMES {
            let path = directory.join(name);
            if path.is_file() {
                self.load_file(&path);
            }
        }
        self.entries.push(ConfigEntry::directory(
            directory.to_string_lossy().into_owned(),
        ));
    }

    fn load_file(&mut self, path: &Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                self.diagnostics.push(Diagnostic::new(
                    Some(&path.to_string_lossy()),
                    DiagnosticKind::Io,
                    "$",
                    format!("failed to read configuration: {error}"),
                ));
                return;
            }
        };
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let source = path.to_string_lossy().into_owned();
        self.parse_and_push(Some(&source), &text, dir, Some(path));
    }

    fn parse_and_push(
        &mut self,
        source: Option<&str>,
        text: &str,
        dir: &Path,
        path: Option<&Path>,
    ) {
        let env = self.env.clone();
        let lookup = |name: &str| env.get(name);
        let home = self.discovery.home.clone();
        let (text, missing) = substitute::substitute(text, dir, &home, &lookup);
        for file in missing {
            self.diagnostics.push(Diagnostic::new(
                source,
                DiagnosticKind::Io,
                "$",
                format!(
                    "file reference {} is missing or unreadable: {}",
                    file.reference,
                    file.path.to_string_lossy()
                ),
            ));
        }
        let parsed = match parse_jsonc(&text) {
            Ok(value) => value,
            Err(error) => {
                self.diagnostics.push(Diagnostic::new(
                    source,
                    DiagnosticKind::Invalid,
                    "$",
                    format!("rejected malformed JSON or JSONC document: {error}"),
                ));
                return;
            }
        };
        let (info, diagnostics) = normalize::normalize(source, parsed);
        self.diagnostics.extend(diagnostics);
        if let Some(info) = info {
            self.documents.push(info.clone());
            self.entries.push(ConfigEntry::document(
                path.map(|path| path.to_string_lossy().into_owned()),
                info,
            ));
        }
    }
}

fn global_permissions(documents: &[Value]) -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    for document in documents {
        let Some(values) = document.get("permissions").and_then(Value::as_array) else {
            continue;
        };
        for value in values {
            let Some(object) = value.as_object() else {
                continue;
            };
            let action = object.get("action").and_then(Value::as_str);
            let resource = object.get("resource").and_then(Value::as_str);
            let effect = object
                .get("effect")
                .and_then(Value::as_str)
                .and_then(Effect::parse);
            if let (Some(action), Some(resource), Some(effect)) = (action, resource, effect) {
                rules.push(PermissionRule::new(action, resource, effect));
            }
        }
    }
    rules
}

fn build_agents(
    documents: &[Value],
    permissions: &[PermissionRule],
    home: &Path,
    markdown: &[markdown_agents::MarkdownAgent],
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<AgentState>, String) {
    let mut agents = agents::builtins(home);
    for agent in &mut agents {
        agent.push_document_permissions(permissions, home);
    }

    for document in documents {
        let Some(entries) = document.get("agents").and_then(Value::as_object) else {
            continue;
        };
        for (id, item) in entries {
            let Some(item) = item.as_object() else {
                continue;
            };
            if item.get("disabled").and_then(Value::as_bool) == Some(true) {
                agents.retain(|agent| agent.id != *id);
                continue;
            }
            let state = agents::resolve(&mut agents, id, home, permissions);
            state.apply(item, home);
        }
    }

    agents::apply_markdown(&mut agents, markdown, home, permissions, diagnostics);

    let configured = documents
        .iter()
        .rev()
        .find_map(|document| document.get("default_agent").and_then(Value::as_str));
    let select = |id: &str| {
        agents
            .iter()
            .find(|agent| agent.id == id && agent.is_selectable())
            .map(|agent| agent.id.clone())
    };
    let default_agent = configured
        .and_then(select)
        .or_else(|| select("build"))
        .or_else(|| {
            agents
                .iter()
                .find(|agent| agent.is_selectable())
                .map(|agent| agent.id.clone())
        })
        .unwrap_or_else(|| "build".to_owned());

    (agents, default_agent)
}

/// Configuration directories in load order (the roots that contribute
/// `skill/`, `command/` and `agent/`), plus the `skills[]` directories from
/// the documents. URL entries are skipped with a diagnostic because loading
/// never touches the network.
fn collect_skill_roots(
    entries: &[ConfigEntry],
    documents: &[Value],
    discovery: &Discovery,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut config_roots = Vec::new();
    let mut seen = HashSet::new();
    for entry in entries {
        if entry.kind == EntryKind::Directory
            && let Some(path) = &entry.path
            && seen.insert(discovery::path_key(Path::new(path)))
        {
            config_roots.push(PathBuf::from(path));
        }
    }

    let mut extra_roots = Vec::new();
    let home = &discovery.home;
    for document in documents {
        let Some(skills) = document.get("skills").and_then(Value::as_array) else {
            continue;
        };
        for skill in skills {
            let Some(skill) = skill.as_str() else {
                continue;
            };
            if skill.starts_with("http://") || skill.starts_with("https://") {
                diagnostics.push(Diagnostic::new(
                    None,
                    DiagnosticKind::Unsupported,
                    "$.skills",
                    format!("skipped URL skill bundle `{skill}`: no network access while loading"),
                ));
                continue;
            }
            let path = PathBuf::from(skill);
            extra_roots.push(if let Some(relative) = skill.strip_prefix("~/") {
                home.join(relative)
            } else if path.is_absolute() {
                path
            } else {
                discovery.location.join(path)
            });
        }
    }

    // Later occurrences win, so deduplicate keeping the last position.
    let mut seen_extra = HashSet::new();
    let mut deduped = Vec::with_capacity(extra_roots.len());
    for root in extra_roots.into_iter().rev() {
        if seen_extra.insert(discovery::path_key(&root)) {
            deduped.push(root);
        }
    }
    deduped.reverse();
    (config_roots, deduped)
}

/// `ConfigService::skills_roots`: config directories followed by `skills[]`
/// directories, without duplicates.
fn visible_skill_roots(config_roots: &[PathBuf], extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(config_roots.len() + extra_roots.len());
    roots.extend(config_roots.iter().cloned());
    roots.extend(extra_roots.iter().cloned());
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(roots.len());
    for root in roots.into_iter().rev() {
        if seen.insert(discovery::path_key(&root)) {
            deduped.push(root);
        }
    }
    deduped.reverse();
    deduped
}

fn default_shell() -> String {
    if cfg!(windows) {
        "pwsh".to_owned()
    } else if cfg!(target_os = "macos") {
        "/bin/zsh".to_owned()
    } else {
        "/bin/bash".to_owned()
    }
}
