//! Lane configuration and concurrency defaults.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Default concurrency for lanes without a built-in or configured value.
pub const DEFAULT_CONCURRENCY: usize = 8;
/// Default maximum number of queued tickets per lane.
pub const DEFAULT_MAX_QUEUE: usize = 256;

/// Named lane concurrency and queue limits.
///
/// Built-in resolution, used when a lane is not present in [`Self::lanes`]:
///
/// | lane | concurrency |
/// |---|---|
/// | `cargo:<workspace>` | 1 |
/// | `node:<workspace>` | 1 |
/// | `network` | 4 |
/// | `gpu` | 1 |
/// | anything else | [`Self::default_concurrency`] |
///
/// An explicit entry always wins. A configured concurrency of zero and a
/// `default_concurrency` of zero are clamped to one rather than silently
/// disabling the lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneConfig {
    /// Per-lane overrides.
    pub lanes: HashMap<String, usize>,
    /// Fallback concurrency.
    pub default_concurrency: usize,
    /// Maximum queued tickets per lane. Zero means unlimited.
    pub max_queue: usize,
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            lanes: HashMap::new(),
            default_concurrency: DEFAULT_CONCURRENCY,
            max_queue: DEFAULT_MAX_QUEUE,
        }
    }
}

impl LaneConfig {
    /// Adds or replaces a lane override.
    #[must_use]
    pub fn with_lane(mut self, lane: impl Into<String>, concurrency: usize) -> Self {
        self.lanes.insert(lane.into(), concurrency);
        self
    }

    /// Resolves the concurrency for a lane.
    ///
    /// Precedence: exact lane key -> `<prefix>` override (e.g. `cargo` for
    /// `cargo:/workspace`) -> built-in default -> `default_concurrency`.
    pub fn concurrency_for(&self, lane: &str) -> usize {
        self.lanes
            .get(lane)
            .copied()
            .filter(|&value| value > 0)
            .or_else(|| {
                lane.split_once(':')
                    .and_then(|(prefix, _)| self.lanes.get(prefix))
                    .copied()
                    .filter(|&value| value > 0)
            })
            .or_else(|| builtin_concurrency(lane))
            .unwrap_or(self.default_concurrency)
            .max(1)
    }
}

fn builtin_concurrency(lane: &str) -> Option<usize> {
    if lane == "network" {
        Some(4)
    } else if lane == "gpu" || lane.starts_with("cargo:") || lane.starts_with("node:") {
        Some(1)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_resolution_honors_exact_then_prefix_then_builtin() {
        let cfg = LaneConfig::default()
            .with_lane("cargo", 2)
            .with_lane("cargo:/special", 3);
        assert_eq!(cfg.concurrency_for("cargo:/special"), 3);
        assert_eq!(cfg.concurrency_for("cargo:/other"), 2);
        assert_eq!(cfg.concurrency_for("node:/other"), 1);
        assert_eq!(cfg.concurrency_for("network"), 4);
        assert_eq!(cfg.concurrency_for("unknown"), DEFAULT_CONCURRENCY);
    }
}
