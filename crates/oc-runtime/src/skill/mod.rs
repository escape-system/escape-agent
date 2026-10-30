//! Skill tools: load a skill, author one, and lint a file (`docs/07`).
//!
//! Loading persists a `skill` message (the prompt uses it to mark the skill as
//! already present) and publishes `session.skill.activated` locally.
//! `skill_write` forces `<scope>/.opencode/skill/<id>/SKILL.md` or the global
//! `<config>/skill/<id>/SKILL.md`, writes atomically, and marks the file with
//! `metadata.opencode/agent: true` so discovery reports
//! `SkillSource::Agent`. `skill_lint` is read-only.

use std::sync::Arc;

use oc_tools::ToolRegistry;

use crate::runtime::Runtime;

mod activate;
mod lint;
mod util;
mod write;

#[cfg(test)]
mod tests;

use activate::SkillTool;
use lint::SkillLintTool;
use write::SkillWriteTool;

const MAX_DESCRIPTION_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 100;
const MAX_SKILL_FILES: usize = 10;
const MAX_SAMPLE_DEPTH: usize = 6;
const SAMPLE_SKIP: [&str; 5] = ["node_modules", ".git", ".venv", "venv", "__pycache__"];

/// Build the skill tool set for one runtime.
#[derive(Clone, Debug)]
pub struct SkillTools {
    runtime: Runtime,
}

impl SkillTools {
    #[must_use]
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }

    /// Registry entries: `skill`, `skill_write` and `skill_lint`.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SkillTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(SkillWriteTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(SkillLintTool {
            runtime: self.runtime.clone(),
        }));
        registry
    }
}
