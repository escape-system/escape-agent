//! Location-scoped VCS routes backed by the real `git` executable.
//!
//! Every invocation is an argv array (no shell) run with `git -C <dir>`, a
//! 10 s deadline and an output cap; the process is killed when either limit is
//! hit. Listing commands run inside the requested location and scope their
//! pathspec to it, while per-file patch commands run at the repository root
//! because git reports root-relative paths.
//!
//! A location outside a repository is not an error: `GET /api/vcs` answers with
//! an empty `branch` object and no `provider`, exactly like upstream's
//! no-provider state, and the change routes report no changes.

pub mod diff;
pub mod git;
pub mod http;
pub mod refs;
pub mod status;

pub use http::{BranchParams, DiffParams, InitParams, base, branches, diff, info, init, status};

use std::time::Duration;

/// Deadline for one git invocation.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Working-copy entries reported by `status` and `diff`.
const MAX_ENTRIES: usize = 2000;
/// Total patch bytes reported by `diff`.
const MAX_DIFF_BYTES: usize = 1024 * 1024;
/// Patch bytes requested for a single untracked file.
const MAX_FILE_PATCH_BYTES: usize = 256 * 1024;
/// Files above this size are reported as binary, like `git --numstat`.
const MAX_STAT_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_CONTEXT: u32 = 3;
const MAX_STDERR: usize = 8 * 1024;
