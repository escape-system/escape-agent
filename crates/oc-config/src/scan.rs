//! Bounded, deterministic directory walking shared by skill, command and
//! markdown-agent discovery.
//!
//! Traversal is breadth-first, depth-limited (docs/07: depth ≤ 6), sorted by
//! path, and skips dot entries plus the heavy directories that never contain
//! authored skills.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Maximum directory depth below a scan root (root entries are depth 0).
pub(crate) const MAX_DEPTH: usize = 6;

const SKIP: [&str; 5] = ["node_modules", ".git", ".venv", "venv", "__pycache__"];

#[derive(Debug, Clone)]
pub(crate) struct Visited {
    pub path: PathBuf,
    /// Depth of the entry below the scan root; direct children are depth 0.
    pub depth: usize,
    pub is_dir: bool,
}

/// List `root` breadth-first. `stop` prevents descending into a directory
/// (a skill directory with `SKILL.md` stops recursion).
pub(crate) fn walk(
    root: &Path,
    max_depth: usize,
    mut stop: impl FnMut(&Path) -> bool,
) -> Vec<Visited> {
    let mut visited = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0usize));
    while let Some((directory, depth)) = queue.pop_front() {
        let Ok(listing) = std::fs::read_dir(&directory) else {
            continue;
        };
        let mut entries: Vec<(PathBuf, bool)> = listing
            .flatten()
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                Some((entry.path(), file_type.is_dir()))
            })
            .collect();
        entries.sort();
        for (path, is_dir) in entries {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') || SKIP.contains(&name) {
                continue;
            }
            visited.push(Visited {
                path: path.clone(),
                depth,
                is_dir,
            });
            if is_dir && depth < max_depth && !stop(&path) {
                queue.push_back((path, depth + 1));
            }
        }
    }
    visited
}

#[must_use]
pub(crate) fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

#[must_use]
pub(crate) fn file_stem(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_owned)
}

#[must_use]
pub(crate) fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}
