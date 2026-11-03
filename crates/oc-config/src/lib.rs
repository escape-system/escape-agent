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
use std::sync::{Arc, RwLock};

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

/// Upper bound on cached per-directory instruction sets. Reaching it clears the
/// map instead of evicting one entry; a re-walk is cheap and correctness does
/// not depend on the cache.
const MAX_INSTRUCTION_CACHE: usize = 128;

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
    /// Instruction files discovered from the load directory. Shared so
    /// [`ConfigService::instructions_for`] can hand it out without cloning.
    instructions: Arc<Vec<InstructionFile>>,
    /// Per-directory instruction discovery, keyed by [`discovery::path_key`] so
    /// `dir`, `dir/` and `dir/.` share one entry.
    ///
    /// Discovery walks the filesystem (up to three `is_file`/`metadata` probes
    /// per ancestor), so it runs at most once per directory per process.
    /// Entries are stable until [`ConfigService::invalidate_instructions`].
    instruction_cache: RwLock<HashMap<String, Arc<Vec<InstructionFile>>>>,
    /// Per-directory skill discovery, keyed like [`Self::instruction_cache`].
    /// This scan is heavier than the instruction walk (it walks every skill
    /// root), so the cache matters more here. Dropped by
    /// [`ConfigService::invalidate_skills`].
    skill_cache: RwLock<HashMap<String, Arc<Vec<SkillInfo>>>>,
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
        let instructions = Arc::new(instructions::discover(
            &discovery.location,
            &mut diagnostics,
        ));
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
            instruction_cache: RwLock::new(HashMap::new()),
            skill_cache: RwLock::new(HashMap::new()),
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
        let (config_roots, extra_roots) = self.skill_roots();
        skills::discover(
            &config_roots,
            &extra_roots,
            &self.discovery,
            &mut diagnostics,
        )
    }

    /// Skills visible from `directory`, using the same roots and precedence as
    /// [`ConfigService::skills`].
    ///
    /// Scoping is the point: a server started in one repository must not hand
    /// its `.opencode/skill/*` to sessions whose `location.directory` is another
    /// project. Project-scope roots outside the session tree are dropped and
    /// the session tree's own `.opencode` directories are scanned, so the index
    /// matches the project the session actually works in. Global configuration
    /// roots, configured `skills[]` directories and the per-user
    /// `.claude`/`.agents` roots stay visible: they are not project-scoped.
    ///
    /// An empty `directory` returns the load-time snapshot. A relative path is
    /// resolved against the load directory. The result is cached per directory
    /// until [`ConfigService::invalidate_skills`]; a skill file written after
    /// the first call is picked up only after invalidation (skill ids already
    /// loaded in a session are unaffected, since the index is frozen into the
    /// instruction prefix).
    #[must_use]
    pub fn skills_for(&self, directory: &Path) -> Arc<Vec<SkillInfo>> {
        if directory.as_os_str().is_empty() {
            return Arc::new(self.skills.clone());
        }
        let resolved = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.discovery.location.join(directory)
        };
        let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
        let key = discovery::path_key(&resolved);
        if key == discovery::path_key(&self.discovery.location) {
            return Arc::new(self.skills.clone());
        }
        if let Some(cached) = self.cached_skills(&key) {
            return cached;
        }

        let mut diagnostics = Vec::new();
        let (config_roots, extra_roots) = self.skill_roots();
        let found = Arc::new(skills::discover_for(
            &config_roots,
            &extra_roots,
            &self.discovery,
            &resolved,
            &mut diagnostics,
        ));
        // Session-scoped diagnostics stay off the wire, like instructions_for.
        drop(diagnostics);
        self.cache_skills(key, Arc::clone(&found));
        found
    }

    /// Drop every cached per-directory skill set, keeping the load-time
    /// snapshot. Call after writing a `SKILL.md` so the next turn's index sees
    /// it.
    pub fn invalidate_skills(&self) {
        let mut cache = self
            .skill_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }

    fn skill_roots(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut diagnostics = Vec::new();
        collect_skill_roots(
            &self.entries,
            &self.documents,
            &self.discovery,
            &mut diagnostics,
        )
    }

    fn cached_skills(&self, key: &str) -> Option<Arc<Vec<SkillInfo>>> {
        let cache = self
            .skill_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    }

    fn cache_skills(&self, key: String, skills: Arc<Vec<SkillInfo>>) {
        let mut cache = self
            .skill_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Same bound as the instruction cache: clear rather than evict one.
        if cache.len() >= MAX_INSTRUCTION_CACHE && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, skills);
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
    ///
    /// This is the load-time snapshot for the directory passed to
    /// [`ConfigService::load`]. Turns must call
    /// [`ConfigService::instructions_for`] with the session directory so a
    /// session in another project never inherits this snapshot.
    #[must_use]
    pub fn instructions(&self) -> &[InstructionFile] {
        self.instructions.as_slice()
    }

    /// Instruction files for `directory`, using the same upward walk and
    /// precedence as [`ConfigService::instructions`].
    ///
    /// Scoping is the point: a server started in one repository must not hand
    /// its `AGENTS.md` to sessions whose `location.directory` is another
    /// project. An empty or relative `directory` is resolved against the load
    /// directory (relative) or falls back to the load-time snapshot (empty), so
    /// the historical behavior is preserved when the session has no directory
    /// of its own.
    ///
    /// The result is cached per directory. Adding, removing or renaming an
    /// `AGENTS.md` after the first call is picked up only after
    /// [`ConfigService::invalidate_instructions`]; edited file contents are
    /// re-read by the runtime on every turn and are not stale.
    #[must_use]
    pub fn instructions_for(&self, directory: &Path) -> Arc<Vec<InstructionFile>> {
        if directory.as_os_str().is_empty() {
            return Arc::clone(&self.instructions);
        }
        let resolved = if directory.is_absolute() {
            directory.to_path_buf()
        } else {
            self.discovery.location.join(directory)
        };
        let resolved = std::path::absolute(&resolved).unwrap_or(resolved);
        let key = discovery::path_key(&resolved);
        if key == discovery::path_key(&self.discovery.location) {
            return Arc::clone(&self.instructions);
        }
        if let Some(cached) = self.cached_instructions(&key) {
            return cached;
        }

        let mut diagnostics = Vec::new();
        let files = Arc::new(instructions::discover(&resolved, &mut diagnostics));
        // Session-scoped diagnostics stay off the wire: `GET /api/config`
        // reports the load-time set, and a turn must not mutate it.
        drop(diagnostics);
        self.cache_instructions(key, Arc::clone(&files));
        files
    }

    /// Drop every cached per-directory instruction set, keeping the load-time
    /// snapshot. Call after the host reloads a location or writes an
    /// instruction file, so the next turn re-walks the filesystem.
    pub fn invalidate_instructions(&self) {
        let mut cache = self
            .instruction_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.clear();
    }

    fn cached_instructions(&self, key: &str) -> Option<Arc<Vec<InstructionFile>>> {
        let cache = self
            .instruction_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.get(key).cloned()
    }

    fn cache_instructions(&self, key: String, files: Arc<Vec<InstructionFile>>) {
        let mut cache = self
            .instruction_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Bound the map: a long-lived server sees one entry per project
        // directory. Clearing is cheaper than an LRU and only costs a re-walk.
        if cache.len() >= MAX_INSTRUCTION_CACHE && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, files);
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
