//! Session-tree rows and their policy-relevant enums.

/// The policy role of a session. It is a hint, not a security boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    Primary,
    Subagent,
    Coordinator,
}

impl Role {
    /// Parse the `session_v2.role` column.
    #[must_use]
    pub fn from_column(value: &str) -> Option<Self> {
        match value {
            "primary" => Some(Self::Primary),
            "subagent" => Some(Self::Subagent),
            "coordinator" => Some(Self::Coordinator),
            _ => None,
        }
    }

    /// The `session_v2.role` column value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Subagent => "subagent",
            Self::Coordinator => "coordinator",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state used by policy decisions and the session tree UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeState {
    Idle,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl NodeState {
    /// Map the wire session status (`idle`, `busy`, `retry`, durable outcomes)
    /// onto a tree state.
    #[must_use]
    pub fn from_status(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "busy" | "running" | "retry" => Some(Self::Running),
            "completed" | "succeeded" => Some(Self::Completed),
            "failed" | "error" => Some(Self::Failed),
            "cancelled" | "canceled" | "interrupted" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Stable label for logs and the tree API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of the session tree, combining `Session.Info` with the columns the
/// server adds (`root_id`, `depth`, `role`, `spawn_allowed`, `child_count`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub root_id: String,
    pub depth: u32,
    pub role: Role,
    pub spawn_allowed: bool,
    pub child_count: u32,
    pub agent: String,
    pub model: Option<String>,
    pub state: NodeState,
}

impl SessionNode {
    /// A root primary session; convenient for tests and root bootstrap.
    #[must_use]
    pub fn primary(id: impl Into<String>, agent: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            root_id: id.clone(),
            id,
            parent_id: None,
            depth: 0,
            role: Role::Primary,
            spawn_allowed: true,
            child_count: 0,
            agent: agent.into(),
            model: None,
            state: NodeState::Idle,
        }
    }

    /// A child of `parent`, deriving root/depth the way the spawn path does.
    #[must_use]
    pub fn child(
        id: impl Into<String>,
        parent: &Self,
        role: Role,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            parent_id: Some(parent.id.clone()),
            root_id: parent.root_id.clone(),
            depth: parent.depth.saturating_add(1),
            role,
            spawn_allowed: parent.spawn_allowed,
            child_count: 0,
            agent: agent.into(),
            model: None,
            state: NodeState::Idle,
        }
    }

    /// True when this session has no parent.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}
