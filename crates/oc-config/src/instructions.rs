//! Project instruction files (`AGENTS.md`, `CLAUDE.md`, `.opencode/AGENTS.md`).
//!
//! The walk starts at the location directory and goes lexically upward to the
//! filesystem root, so the outermost instructions come first (most shared →
//! least shared, docs/04). Duplicates reached through different paths are
//! deduplicated by canonical path. Files over 1 MiB are skipped.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::discovery;

/// Per-file cap: instruction files are prompt material, not data dumps.
const MAX_INSTRUCTION_BYTES: u64 = 1024 * 1024;

const NAMES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", ".opencode/AGENTS.md"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub text: String,
}

pub(crate) fn discover(location: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<InstructionFile> {
    let mut directories = discovery::ancestors(location);
    directories.reverse();
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for directory in directories {
        for name in NAMES {
            let path = directory.join(name);
            if !path.is_file() {
                continue;
            }
            let key = std::fs::canonicalize(&path).map_or_else(
                |_| discovery::path_key(&path),
                |canonical| discovery::path_key(&canonical),
            );
            if !seen.insert(key) {
                continue;
            }
            let source = path.to_string_lossy().into_owned();
            let size = std::fs::metadata(&path).map(|metadata| metadata.len());
            if let Ok(size) = size
                && size > MAX_INSTRUCTION_BYTES
            {
                diagnostics.push(Diagnostic::new(
                    Some(&source),
                    DiagnosticKind::Invalid,
                    "$",
                    format!("instruction file exceeds the 1 MiB cap ({size} bytes); skipped"),
                ));
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    diagnostics.push(Diagnostic::new(
                        Some(&source),
                        DiagnosticKind::Io,
                        "$",
                        format!("failed to read instruction file: {error}"),
                    ));
                    continue;
                }
            };
            files.push(InstructionFile { path, text });
        }
    }
    files
}
