//! Lane integration: the process runner, the lane-aware `shell` tool and the
//! coordinator `lane` tool.
//!
//! [`ProcessLaneRunner`] executes tickets for [`oc_lanes::LaneService`]. The
//! service assigns tickets to lanes with semaphores; the runner only spawns the
//! configured shell, captures combined stdout/stderr into a temp file and
//! posts the file path back through `RunnerResult::output_path`.
//!
//! [`ShellLaneTool`] replaces the plain `shell` tool when a lane service is
//! attached. Build/test commands are classified with [`oc_lanes::classify`] and
//! submitted as tickets; everything else delegates to
//! [`oc_tools::ShellTool`] unchanged.
//!
//! [`LaneTool`] is the coordinator surface (`queue`, `order`, `status`,
//! `cancel`, `notify`). Permission mapping belongs to the host: it authorises
//! calls to this tool under the `lane` action.

use std::hash::{Hash as _, Hasher as _};
use std::time::Duration;

use oc_tools::{ToolError, ToolRegistry};
use serde_json::Value;

mod output;
mod runner;
mod shell_tool;
mod tool;

pub use runner::ProcessLaneRunner;
pub use shell_tool::ShellLaneTool;
pub use tool::LaneTool;

/// Default lane wait (5 minutes) when the caller does not pass `timeout`.
pub const DEFAULT_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
/// Maximum number of trailing lines returned by the lane tools.
pub const MAX_LINES: usize = 2_000;
/// Maximum number of trailing bytes returned by the lane tools.
pub const MAX_BYTES: usize = 50 * 1024;
/// Longest timeout a caller can express; `timeout: 0` means this.
const NO_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);
const LANE_DIR: &str = "opencode-lanes";
/// Per-probe time budget for the two `git` calls that build a tree revision.
/// A hung git must never stall a lane submission.
const TREE_REV_GIT_BUDGET: Duration = Duration::from_millis(500);

/// Best-effort tree revision used to coalesce identical lane work.
///
/// The value is `<short HEAD>+<016x hash>`, where the hash covers the short
/// revision and `git status --porcelain=v2 --untracked-files=no`. Two tickets
/// coalesce only when they run the same command in the same working directory
/// at the same commit with the same tracked-tree state.
///
/// Failure is never fatal: no git binary, a non-repository `cwd`, a timeout
/// (each probe is bounded by [`TREE_REV_GIT_BUDGET`]) or any non-zero exit all
/// yield `None`, which disables coalescing for that ticket. A lane submission
/// never fails because git did.
pub(crate) async fn tree_rev(cwd: &str) -> Option<String> {
    let head = git_probe(cwd, &["rev-parse", "--short", "HEAD"]).await?;
    let status = git_probe(cwd, &["status", "--porcelain=v2", "--untracked-files=no"]).await?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    head.as_bytes().hash(&mut hasher);
    status.as_bytes().hash(&mut hasher);
    Some(format!("{head}+{:016x}", hasher.finish()))
}

async fn git_probe(cwd: &str, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    let output = tokio::time::timeout(TREE_REV_GIT_BUDGET, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_owned())
}

fn optional_i64(input: &Value, key: &str) -> Result<Option<i64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number.as_i64().map(Some).ok_or_else(|| {
            ToolError::new(format!("Invalid arguments: `{key}` must be an integer"))
        }),
        Some(_) => Err(ToolError::new(format!(
            "Invalid arguments: `{key}` must be an integer"
        ))),
    }
}

// ---------------------------------------------------------------------------

fn clamp_priority(priority: i64) -> i32 {
    i32::try_from(priority).unwrap_or(if priority < 0 { i32::MIN } else { i32::MAX })
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Registry factory for the lane-aware tools.
#[derive(Debug, Clone)]
pub struct LaneTools {
    runtime: crate::Runtime,
}

impl LaneTools {
    /// Tools bound to one runtime.
    #[must_use]
    pub fn new(runtime: crate::Runtime) -> Self {
        Self { runtime }
    }

    /// Registry containing the `shell` replacement and the `lane` tool.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(std::sync::Arc::new(ShellLaneTool::new(
            self.runtime.clone(),
        )));
        registry.register(std::sync::Arc::new(LaneTool::new(self.runtime.clone())));
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::tree_rev;

    /// `None` when `cwd` is not inside a git repository: coalescing must be
    /// disabled rather than guessed.
    #[tokio::test]
    async fn tree_rev_is_none_outside_a_repository() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(tree_rev(&dir.path().to_string_lossy()).await, None);
    }

    /// A clean repository yields `<short HEAD>+<hex>`; changing a tracked file
    /// changes the revision, and an identical state yields the identical value.
    #[tokio::test]
    async fn tree_rev_tracks_commit_and_tracked_changes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path();
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .output()
                .expect("git runs");
            assert!(output.status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet"]);
        std::fs::write(path.join("tracked.txt"), "one\n").expect("write");
        run(&["add", "tracked.txt"]);
        run(&["commit", "--quiet", "-m", "initial"]);

        let clean = tree_rev(&path.to_string_lossy()).await.expect("clean rev");
        assert!(clean.contains('+'), "{clean}");
        assert_eq!(
            Some(clean.clone()),
            tree_rev(&path.to_string_lossy()).await,
            "an unchanged tree is stable"
        );

        std::fs::write(path.join("tracked.txt"), "two\n").expect("dirty");
        let dirty = tree_rev(&path.to_string_lossy()).await.expect("dirty rev");
        assert_ne!(clean, dirty, "a tracked change must change the revision");

        // An untracked file is deliberately ignored: it cannot change a build.
        std::fs::write(path.join("untracked.txt"), "noise\n").expect("untracked");
        assert_eq!(
            Some(dirty),
            tree_rev(&path.to_string_lossy()).await,
            "untracked files do not affect the revision"
        );
    }
}
