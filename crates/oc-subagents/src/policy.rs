//! The policy object: one immutable [`Limits`] snapshot, explicit decisions.

use crate::message::{TreeLookup, message_decision, write_decision};
use crate::node::SessionNode;
use crate::spawn::{ChildPlan, spawn_decision};
use crate::{Limits, MessageError, SpawnError, WriteError};

/// Session-tree policy. Construct once per config snapshot; it holds no state
/// and performs no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    limits: Limits,
}

impl Policy {
    #[must_use]
    pub const fn new(limits: Limits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Decide whether `parent` may create a child for `requested_agent`.
    ///
    /// Checks, in order: global enable, `parent.spawn_allowed`, depth
    /// (`requested_depth` may only lower the configured ceiling), the child
    /// count, agent availability and primary-only agents.
    pub fn spawn(
        &self,
        parent: &SessionNode,
        requested_agent: &str,
        requested_depth: Option<u32>,
        available_agents: &[String],
    ) -> Result<ChildPlan, SpawnError> {
        spawn_decision(
            &self.limits,
            parent,
            requested_agent,
            requested_depth,
            available_agents,
        )
    }

    /// Authorize a `talk` message from `from` to `to`.
    pub fn can_message(
        &self,
        from: &SessionNode,
        to: &SessionNode,
        tree: &dyn TreeLookup,
    ) -> Result<(), MessageError> {
        message_decision(&self.limits, from, to, tree)
    }

    /// Authorize a `session_write` from `from` to `to`.
    pub fn can_write(
        &self,
        from: &SessionNode,
        to: &SessionNode,
        tree: &dyn TreeLookup,
    ) -> Result<(), WriteError> {
        write_decision(from, to, tree)
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}
