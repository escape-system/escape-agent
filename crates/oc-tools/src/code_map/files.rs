//! Workspace file discovery and bounded reads shared by the `code_map` operations.

use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::{ToolContext, ToolError};

const MAX_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024; // 4 MB cap

/// Helper to read a file up to `MAX_FILE_SIZE_BYTES`.
pub(super) fn read_bounded_file(path: &Path) -> Result<String, ToolError> {
    let metadata = fs::metadata(path).map_err(|e| ToolError::new(e.to_string()))?;
    if metadata.len() > MAX_FILE_SIZE_BYTES {
        return Err(ToolError::new(format!(
            "File exceeds maximum analysis size (4 MB): {}",
            path.display()
        )));
    }
    fs::read_to_string(path).map_err(|e| ToolError::new(e.to_string()))
}

/// Collect source files respecting .gitignore, ignoring hidden directories.
pub(super) fn collect_code_files(
    ctx: &ToolContext,
    root: &Path,
) -> Result<Vec<PathBuf>, ToolError> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .parents(true)
        .build();

    for entry in walker {
        if let Some(ref cancel) = ctx.cancel
            && cancel.is_cancelled()
        {
            return Err(ToolError::new("Tool execution cancelled"));
        }
        let Ok(entry) = entry else { continue };
        let p = entry.path();
        if p.is_file() && is_code_extension(p) {
            files.push(p.to_path_buf());
        }
    }
    Ok(files)
}

pub(super) fn is_code_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "py"
            | "go"
            | "c"
            | "cpp"
            | "cc"
            | "h"
            | "hpp"
            | "md"
            | "toml"
            | "json"
    )
}

pub(super) fn relative_to(root: &Path, target: &Path) -> String {
    if let Ok(rel) = target.strip_prefix(root) {
        rel.to_string_lossy().into_owned()
    } else {
        target.to_string_lossy().into_owned()
    }
}
