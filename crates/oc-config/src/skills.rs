//! Skill discovery.
//!
//! Scan roots are the config directories in load order (each contributing
//! `skill/` then `skills/`), any `skills[]` directories from the merged
//! documents, and the Claude/agents compatibility roots. Skills are never
//! executed; the body is instructions (docs/07).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::discovery::{self, Discovery};
use crate::markdown;
use crate::scan;

const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub autoinvoke: Option<bool>,
    pub path: PathBuf,
    pub content: String,
    pub source: SkillSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Config directory (`global`, managed/system) or an explicit `skills[]` path.
    Config,
    /// A project `.opencode` directory.
    Project,
    /// `~/.claude/skills` or a project `.claude/skills`.
    Claude,
    /// `~/.agents/skills` or a project `.agents/skills`.
    Agents,
    /// Written by `skill_write`: frontmatter carries
    /// `metadata.opencode/agent: true`.
    Agent,
    /// Reserved for embedded built-in skills.
    Builtin,
}

/// Discover every skill reachable from the configured roots and the
/// compatibility roots. Later occurrences of an id win; the result is sorted
/// by id.
pub(crate) fn discover(
    config_roots: &[PathBuf],
    extra_roots: &[PathBuf],
    discovery: &Discovery,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<SkillInfo> {
    collect(
        config_roots,
        extra_roots,
        discovery,
        &discovery.location,
        false,
        diagnostics,
    )
}

/// Discover skills as seen from `location` rather than the load directory.
///
/// Used by [`crate::ConfigService::skills_for`]: a session in another project
/// must not inherit the server directory's project skills. Global
/// configuration roots, `skills[]` entries and the per-user compatibility
/// directories stay visible because they are not project-scoped; project
/// `.opencode` roots and `.claude`/`.agents` ancestors are resolved from
/// `location` so only the session's own tree contributes them.
pub(crate) fn discover_for(
    config_roots: &[PathBuf],
    extra_roots: &[PathBuf],
    discovery: &Discovery,
    location: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<SkillInfo> {
    collect(
        config_roots,
        extra_roots,
        discovery,
        location,
        true,
        diagnostics,
    )
}

fn collect(
    config_roots: &[PathBuf],
    extra_roots: &[PathBuf],
    discovery: &Discovery,
    location: &Path,
    scoped: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<SkillInfo> {
    let mut targets: Vec<(PathBuf, SkillSource)> = Vec::new();
    // 1. Global config roots.
    for root in config_roots {
        let source = source_for(root, discovery);
        if source != SkillSource::Project {
            targets.push((root.join("skill"), source));
            targets.push((root.join("skills"), source));
        }
    }

    // 2. Global compatibility roots.
    let mut ancestors = discovery::ancestors(location);
    ancestors.reverse();
    let home = &discovery.home;
    if !home.as_os_str().is_empty() {
        targets.push((home.join(".claude").join("skills"), SkillSource::Claude));
        targets.push((home.join(".agents").join("skills"), SkillSource::Agents));
    }
    for directory in &ancestors {
        targets.push((
            directory.join(".claude").join("skills"),
            SkillSource::Claude,
        ));
        targets.push((
            directory.join(".agents").join("skills"),
            SkillSource::Agents,
        ));
    }

    // 3. Project-scoped config roots.
    for root in config_roots {
        let source = source_for(root, discovery);
        if source == SkillSource::Project {
            if !shares_tree(location, root) {
                continue;
            }
            targets.push((root.join("skill"), source));
            targets.push((root.join("skills"), source));
        }
    }

    // 4. Scoped ancestor project trees.
    if scoped {
        for directory in &ancestors {
            let candidate = directory.join(".opencode");
            if candidate.is_dir() {
                targets.push((candidate.join("skill"), SkillSource::Project));
                targets.push((candidate.join("skills"), SkillSource::Project));
            }
        }
    }

    // Configured `skills[]` directories have the highest precedence.
    for root in extra_roots {
        targets.push((root.clone(), source_for(root, discovery)));
    }

    // The same directory may be reachable several ways; the last appearance
    // wins, matching file-level precedence.
    let mut seen = HashSet::new();
    let mut deduped: Vec<(PathBuf, SkillSource)> = Vec::new();
    for target in targets.into_iter().rev() {
        if seen.insert(discovery::path_key(&target.0)) {
            deduped.push(target);
        }
    }
    deduped.reverse();

    let mut skills: Vec<SkillInfo> = Vec::new();
    for (root, source) in deduped {
        if root.is_dir() {
            scan_root(&root, source, &mut skills, diagnostics);
        }
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    skills
}

/// Whether `root` is an ancestor of `location` or lives inside it. Only such a
/// root's configuration applies to the session.
fn shares_tree(location: &Path, root: &Path) -> bool {
    discovery::is_under(location, root) || discovery::is_under(root, location)
}

fn source_for(root: &Path, discovery: &Discovery) -> SkillSource {
    if discovery
        .project
        .iter()
        .any(|project| discovery::is_under(root, project))
    {
        SkillSource::Project
    } else {
        SkillSource::Config
    }
}

fn scan_root(
    root: &Path,
    source: SkillSource,
    skills: &mut Vec<SkillInfo>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // A directory containing `SKILL.md` is one skill; its children are part of
    // the same skill, not additional skills.
    let stop = |path: &Path| path.join(SKILL_FILE).is_file();
    for entry in scan::walk(root, scan::MAX_DEPTH, stop) {
        if entry.is_dir {
            let skill_file = entry.path.join(SKILL_FILE);
            if skill_file.is_file()
                && let Some(id) = scan::file_name(&entry.path)
            {
                load(&skill_file, id, source, skills, diagnostics);
            }
        } else if entry.depth == 0
            && scan::is_markdown(&entry.path)
            && let Some(id) = scan::file_stem(&entry.path)
        {
            load(&entry.path, id, source, skills, diagnostics);
        }
    }
}

fn load(
    path: &Path,
    id: String,
    source: SkillSource,
    skills: &mut Vec<SkillInfo>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let source_label = path.to_string_lossy().into_owned();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(Diagnostic::new(
                Some(&source_label),
                DiagnosticKind::Io,
                "$",
                format!("failed to read skill: {error}"),
            ));
            return;
        }
    };
    let parsed = markdown::parse(&text);
    if let Some(error) = &parsed.error {
        diagnostics.push(Diagnostic::new(
            Some(&source_label),
            DiagnosticKind::Invalid,
            "$.frontmatter",
            format!("unparsable frontmatter: {error}"),
        ));
    }
    let name = parsed
        .meta
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source = if agent_authored(&parsed.meta) {
        SkillSource::Agent
    } else {
        source
    };
    if name.is_none() {
        diagnostics.push(Diagnostic::new(
            Some(&source_label),
            DiagnosticKind::Invalid,
            "$.name",
            format!("skill `{id}` has no frontmatter `name`; using the id"),
        ));
    }
    let skill = SkillInfo {
        name: name.unwrap_or_else(|| id.clone()),
        id,
        description: parsed
            .meta
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        autoinvoke: autoinvoke(&parsed.meta),
        path: path.to_path_buf(),
        content: parsed.body,
        source,
    };
    if let Some(existing) = skills.iter_mut().find(|item| item.id == skill.id) {
        diagnostics.push(Diagnostic::new(
            Some(&source_label),
            DiagnosticKind::Conflict,
            "$.id",
            format!(
                "duplicate skill id `{}`; the later definition wins",
                skill.id
            ),
        ));
        *existing = skill;
    } else {
        skills.push(skill);
    }
}

/// Skills written by `skill_write` carry `metadata.opencode/agent: true`.
fn agent_authored(meta: &Map<String, Value>) -> bool {
    meta.get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("opencode/agent"))
        .and_then(markdown::coerce_bool)
        == Some(true)
}

/// `metadata["opencode/autoinvoke"]` first, then `disable-model-invocation`.
fn autoinvoke(meta: &Map<String, Value>) -> Option<bool> {
    if let Some(value) = meta
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("opencode/autoinvoke"))
        && let Some(value) = markdown::coerce_bool(value)
    {
        return Some(value);
    }
    match meta
        .get("disable-model-invocation")
        .and_then(markdown::coerce_bool)
    {
        Some(true) => Some(false),
        Some(false) | None => None,
    }
}
