//! Slash-command discovery from markdown files.
//!
//! Each config root contributes `command/` and `commands/`, scanned
//! recursively. The file stem is the command name, the body is the template
//! and the frontmatter carries `description`/`agent`/`model`/`subtask` (with
//! `subagent` accepted as the upstream alias).

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::markdown;
use crate::normalize;
use crate::scan;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub subtask: Option<bool>,
    pub template: String,
    pub path: PathBuf,
}

/// Discover commands from every config root. Later roots win; results are
/// sorted by name.
pub(crate) fn discover(roots: &[PathBuf], diagnostics: &mut Vec<Diagnostic>) -> Vec<CommandInfo> {
    let mut commands: Vec<CommandInfo> = Vec::new();
    for root in roots {
        for directory in ["command", "commands"] {
            scan_directory(&root.join(directory), &mut commands, diagnostics);
        }
    }
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    commands
}

fn scan_directory(
    directory: &Path,
    commands: &mut Vec<CommandInfo>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !directory.is_dir() {
        return;
    }
    for entry in scan::walk(directory, scan::MAX_DEPTH, |_| false) {
        if !entry.is_dir
            && scan::is_markdown(&entry.path)
            && let Some(name) = scan::file_stem(&entry.path)
            && let Some(command) = load(&entry.path, name, diagnostics)
        {
            if let Some(existing) = commands.iter_mut().find(|item| item.name == command.name) {
                let source = command.path.to_string_lossy().into_owned();
                diagnostics.push(Diagnostic::new(
                    Some(&source),
                    DiagnosticKind::Conflict,
                    "$.name",
                    format!(
                        "duplicate command name `{}`; the later definition wins",
                        command.name
                    ),
                ));
                *existing = command;
            } else {
                commands.push(command);
            }
        }
    }
}

fn load(path: &Path, name: String, diagnostics: &mut Vec<Diagnostic>) -> Option<CommandInfo> {
    let source = path.to_string_lossy().into_owned();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            diagnostics.push(Diagnostic::new(
                Some(&source),
                DiagnosticKind::Io,
                "$",
                format!("failed to read command: {error}"),
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
    let string = |key: &str| {
        parsed
            .meta
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let boolean = |key: &str| parsed.meta.get(key).and_then(markdown::coerce_bool);
    Some(CommandInfo {
        name,
        description: string("description"),
        agent: string("agent"),
        model: parsed.meta.get("model").and_then(normalize::model_string),
        subtask: boolean("subtask").or_else(|| boolean("subagent")),
        template: parsed.body,
        path: path.to_path_buf(),
    })
}
