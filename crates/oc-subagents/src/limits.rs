//! Configuration limits and their defaults.

/// The `subagents` config section, already resolved (no config dependency).
#[allow(clippy::struct_excessive_bools)] // config mirrors four independent switches
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub enabled: bool,
    pub max_depth: u32,
    pub max_children: u32,
    pub allow_spawn: bool,
    pub default_model: Option<String>,
    pub default_agent: String,
    pub siblings_enabled: bool,
    pub allow_parent: bool,
    pub max_messages_per_min: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            enabled: true,
            max_depth: 3,
            max_children: 8,
            allow_spawn: true,
            default_model: None,
            default_agent: "general".to_owned(),
            siblings_enabled: true,
            allow_parent: true,
            max_messages_per_min: 20,
        }
    }
}

impl Limits {
    /// Fully permissive limits, useful for tests that isolate one rule.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self {
            enabled: true,
            max_depth: u32::MAX,
            max_children: u32::MAX,
            allow_spawn: true,
            default_model: None,
            default_agent: "general".to_owned(),
            siblings_enabled: true,
            allow_parent: true,
            max_messages_per_min: u32::MAX,
        }
    }
}
