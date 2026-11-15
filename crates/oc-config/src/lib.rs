//! Configuration service: discovery, JSONC parsing, V1 normalization, merging,
//! built-in agents and permission evaluation.
//!
//! Loading never fails because of a single document: unreadable or malformed
//! sources are skipped and recorded in [`ConfigService::diagnostics`]. There is
//! no network access and no write to user configuration.
//!
//! The facade owns the shared types and the re-exports; `service` holds the
//! [`ConfigService`] inherent `impl` split by cluster, `loader` the ordered
//! document walk and `free` the composition helpers.

mod agents;
mod commands;
mod diagnostic;
mod discovery;
mod free;
mod instructions;
mod jsonc;
mod loader;
pub(crate) mod markdown;
mod markdown_agents;
mod merge;
mod normalize;
mod permissions;
mod scan;
mod service;
mod skills;
mod substitute;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use serde_json::{Map, Value};

pub use commands::CommandInfo;
pub use diagnostic::{Diagnostic, DiagnosticKind};
use discovery::Discovery;
pub use instructions::InstructionFile;
pub(crate) use jsonc::parse_jsonc;
/// Frontmatter reader shared with callers that validate a skill file before
/// writing it (`skill_lint`).
pub use markdown::{Parsed as MarkdownDocument, parse as parse_markdown};
pub use permissions::{Effect, PermissionRule, evaluate_rules, wildcard_match};
pub use skills::{SkillInfo, SkillSource};

use agents::AgentState;
pub use agents::normalize_agent_id;

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
    /// Owned `models` control-plane section. Seeded from the merged document at
    /// load, replaced by `PUT /api/config/models` and refreshed from disk.
    ///
    /// It is a cell rather than a field of the merged snapshot because a
    /// `PUT` must be visible to the next spawn without a restart; readers clone
    /// the value out and never hold the lock.
    models: RwLock<ModelsCell>,
}

/// One immutable revision of the `models` section.
///
/// Private to the crate: callers see [`ConfigService::models_section`] and
/// [`ConfigService::models_revision`].
#[derive(Debug, Clone)]
pub(crate) struct ModelsCell {
    pub(crate) value: Value,
    pub(crate) revision: u64,
}
