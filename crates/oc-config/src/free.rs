//! Free functions shared by construction, agent composition and skill roots.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agents::{self, AgentState};
use crate::discovery::{self, Discovery};
use crate::markdown_agents;
use crate::permissions::{Effect, PermissionRule};
use crate::{ConfigEntry, Diagnostic, DiagnosticKind, EntryKind};

pub(super) fn global_permissions(documents: &[Value]) -> Vec<PermissionRule> {
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

pub(super) fn build_agents(
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
        .or_else(|| select("engineer"))
        .or_else(|| {
            agents
                .iter()
                .find(|agent| agent.is_selectable())
                .map(|agent| agent.id.clone())
        })
        .unwrap_or_else(|| "engineer".to_owned());

    (agents, default_agent)
}

/// Configuration directories in load order (the roots that contribute
/// `skill/`, `command/` and `agent/`), plus the `skills[]` directories from
/// the documents. URL entries are skipped with a diagnostic because loading
/// never touches the network.
pub(super) fn collect_skill_roots(
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
pub(super) fn visible_skill_roots(
    config_roots: &[PathBuf],
    extra_roots: &[PathBuf],
) -> Vec<PathBuf> {
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

pub(super) fn default_shell() -> String {
    if cfg!(windows) {
        "pwsh".to_owned()
    } else if cfg!(target_os = "macos") {
        "/bin/zsh".to_owned()
    } else {
        "/bin/bash".to_owned()
    }
}
