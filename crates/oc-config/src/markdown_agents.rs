//! Markdown-defined agents.
//!
//! `{agent,agents}/**/*.md` define agents whose file stem is the id and whose
//! body is the `system` prompt; `{mode,modes}/*.md` are forced to primary.
//! Frontmatter is decoded with the same keys as `Config.Agent` in JSON, and
//! markdown agents are applied after JSON agents (later wins).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::discovery::{self, Discovery};
use crate::markdown;
use crate::scan;

#[derive(Debug, Clone)]
pub(crate) struct MarkdownAgent {
    pub id: String,
    pub path: PathBuf,
    pub frontmatter: Map<String, Value>,
    pub body: String,
    pub forced_mode: Option<String>,
}

pub(crate) fn discover(
    config_roots: &[PathBuf],
    discovery: &Discovery,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<MarkdownAgent> {
    // (directory, only direct children)
    let mut targets: Vec<(PathBuf, bool)> = Vec::new();
    for root in config_roots {
        targets.push((root.join("agent"), false));
        targets.push((root.join("agents"), false));
        targets.push((root.join("mode"), true));
        targets.push((root.join("modes"), true));
    }
    let home = &discovery.home;
    if !home.as_os_str().is_empty() {
        targets.push((home.join(".claude").join("agents"), false));
        targets.push((home.join(".agents").join("agents"), false));
    }
    let mut ancestors = discovery::ancestors(&discovery.location);
    ancestors.reverse();
    for directory in ancestors {
        targets.push((directory.join(".claude").join("agents"), false));
        targets.push((directory.join(".agents").join("agents"), false));
    }

    let mut seen = HashSet::new();
    let mut deduped: Vec<(PathBuf, bool)> = Vec::new();
    for target in targets.into_iter().rev() {
        if seen.insert(discovery::path_key(&target.0)) {
            deduped.push(target);
        }
    }
    deduped.reverse();

    let mut agents: Vec<MarkdownAgent> = Vec::new();
    for (directory, direct_only) in deduped {
        if !directory.is_dir() {
            continue;
        }
        let depth = if direct_only { 0 } else { scan::MAX_DEPTH };
        for entry in scan::walk(&directory, depth, |_| false) {
            if entry.is_dir || !scan::is_markdown(&entry.path) {
                continue;
            }
            if let Some(agent) = load(&entry.path, direct_only, diagnostics) {
                if let Some(existing) = agents.iter_mut().find(|item| item.id == agent.id) {
                    let source = agent.path.to_string_lossy().into_owned();
                    diagnostics.push(Diagnostic::new(
                        Some(&source),
                        DiagnosticKind::Conflict,
                        "$.id",
                        format!(
                            "duplicate markdown agent id `{}`; the later definition wins",
                            agent.id
                        ),
                    ));
                    *existing = agent;
                } else {
                    agents.push(agent);
                }
            }
        }
    }
    agents
}

fn load(
    path: &Path,
    forced_primary: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<MarkdownAgent> {
    let id = scan::file_stem(path)?;
    let source = path.to_string_lossy().into_owned();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(Diagnostic::new(
                Some(&source),
                DiagnosticKind::Io,
                "$",
                format!("failed to read agent: {error}"),
            ));
            return None;
        }
    };
    let parsed = markdown::parse(&text);
    if let Some(error) = &parsed.error {
        diagnostics.push(Diagnostic::new(
            Some(&source),
            DiagnosticKind::Invalid,
            "$.frontmatter",
            format!("unparsable frontmatter: {error}"),
        ));
    }
    Some(MarkdownAgent {
        id,
        path: path.to_path_buf(),
        frontmatter: parsed.meta,
        body: parsed.body,
        forced_mode: forced_primary.then(|| "primary".to_owned()),
    })
}
