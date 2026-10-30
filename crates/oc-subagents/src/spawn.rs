//! Spawn authorization and the resulting child plan.

use crate::node::{Role, SessionNode};
use crate::{Limits, SpawnError};

/// Built-in agents whose config mode is `primary` (or hidden). They cannot be
/// spawned; custom primary agents are filtered by the caller's
/// `available_agents` list, which should contain spawnable ids only.
pub const PRIMARY_ONLY_AGENTS: &[&str] = &["build", "plan", "compaction", "title", "summary"];

/// The policy-approved shape of a child session. The runtime still has to
/// create the row; this is the only path that may do so for a subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPlan {
    pub depth: u32,
    pub role: Role,
    pub spawn_allowed: bool,
    pub agent: String,
}

pub(crate) fn spawn_decision(
    limits: &Limits,
    parent: &SessionNode,
    requested_agent: &str,
    requested_depth: Option<u32>,
    available_agents: &[String],
) -> Result<ChildPlan, SpawnError> {
    if !limits.enabled {
        return Err(SpawnError::Disabled);
    }
    if !parent.spawn_allowed {
        return Err(SpawnError::SpawnNotAllowed {
            id: parent.id.clone(),
        });
    }

    // A call can tighten the effective depth ceiling, never raise it.
    let max_depth = requested_depth.map_or(limits.max_depth, |depth| depth.min(limits.max_depth));
    let depth = parent
        .depth
        .checked_add(1)
        .ok_or(SpawnError::DepthExceeded {
            depth: u32::MAX,
            max: max_depth,
        })?;
    if depth > max_depth {
        return Err(SpawnError::DepthExceeded {
            depth,
            max: max_depth,
        });
    }

    if parent.child_count >= limits.max_children {
        return Err(SpawnError::ChildLimit {
            id: parent.id.clone(),
            children: parent.child_count,
            max: limits.max_children,
        });
    }

    if !available_agents
        .iter()
        .any(|agent| agent.as_str() == requested_agent)
    {
        return Err(SpawnError::UnknownAgent {
            agent: requested_agent.to_owned(),
        });
    }
    if PRIMARY_ONLY_AGENTS.contains(&requested_agent) {
        return Err(SpawnError::PrimaryOnlyAgent {
            agent: requested_agent.to_owned(),
        });
    }

    Ok(ChildPlan {
        depth,
        role: Role::Subagent,
        spawn_allowed: parent.spawn_allowed && limits.allow_spawn,
        agent: requested_agent.to_owned(),
    })
}
