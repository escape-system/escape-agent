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
