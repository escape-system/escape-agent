//! Subagent, sibling and digest tools (`docs/05-subagents.md`,
//! `docs/19-agentic-integration.md` §4).
//!
//! The tools are built per session in `crate::turn` and merged into the
//! per-session registry after the memory tools. Every authorization decision
//! comes from [`oc_subagents::Policy`]; the runtime only supplies store rows,
//! budgets and delivery.
//!
//! # Permission actions
//!
//! `subagent` is authorized as `subagent`. `talk`, `session_write`, `roster`
//! and `digest` are authorized as `read`: they observe/route the session tree
//! but cannot spawn, so the built-in `explore` deny on `subagent` does not
//! silence sibling communication. Built-in `general` keeps spawning enabled;
//! whether it may spawn is governed by `subagents.allow_spawn`, `max_depth` and
//! per-agent overrides. The [`PermissionHandler`] is asked before a tool
//! executes, exactly like every other tool.
//!
//! [`PermissionHandler`]: crate::PermissionHandler

use std::sync::Arc;

use oc_tools::ToolRegistry;

use crate::runtime::Runtime;

mod digest;
pub(crate) mod protocol;
mod roster;
pub(crate) mod settings;
mod spawn;
mod state;
mod talk;
mod tree;
mod wait;
mod write;

pub(crate) use settings::session_subagent_mode;
pub(crate) use state::SubagentState;
pub(crate) use wait::deliver_subagent_result;

use digest::DigestTool;
use roster::RosterTool;
use settings::load_settings;
use spawn::SubagentTool;
use talk::TalkTool;
use write::WriteSessionTool;

/// The subagent tool family, built once per session by the turn loop.
#[derive(Debug, Clone)]
pub struct SubagentTools {
    runtime: Runtime,
}

impl SubagentTools {
    #[must_use]
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }

    /// The five tools: `subagent`, `talk`, `roster`, `session_write`, `digest`.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(SubagentTool {
            runtime: self.runtime.clone(),
            background_default: load_settings(&self.runtime.inner.config.config).background_default,
        }));
        registry.register(Arc::new(TalkTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(RosterTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(WriteSessionTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(DigestTool {
            runtime: self.runtime.clone(),
        }));
        registry
    }
}
