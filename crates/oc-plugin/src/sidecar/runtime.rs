//! Sidecar runtime discovery.
//!
//! The sidecar runs upstream plugin code, which needs Bun APIs. We prefer
//! `bun`, then `node` (with the compatibility shim shipped by the JS host).
//! Discovery is a PATH lookup only: the Rust process never runs npm.

use std::ffi::OsStr;
use std::path::PathBuf;

#[cfg(windows)]
const RUNTIME_SUFFIXES: &[&str] = &[".exe", ".cmd", ".bat"];
#[cfg(not(windows))]
const RUNTIME_SUFFIXES: &[&str] = &[""];

/// Runtime basenames in preference order.
const RUNTIMES: &[&str] = &["bun", "node"];

/// Find `bun` or `node` on the process PATH.
#[must_use]
pub fn detect_runtime() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    detect_runtime_in(&path)
}

/// Find `bun` or `node` on an explicit PATH value. Returns `None` when neither
/// exists; callers mark JS plugins `failed (runtime unavailable)` and keep
/// serving native plugins.
#[must_use]
pub fn detect_runtime_in(path: &OsStr) -> Option<PathBuf> {
    let directories: Vec<PathBuf> = std::env::split_paths(path).collect();
    for runtime in RUNTIMES {
        for directory in &directories {
            for suffix in RUNTIME_SUFFIXES {
                let candidate = directory.join(format!("{runtime}{suffix}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}
