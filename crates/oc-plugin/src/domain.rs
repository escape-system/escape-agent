//! Transform domains.
//!
//! Every domain is a JSON document owned by the config/runtime layer; native
//! and JS plugins mutate the same document, so their effect is observable in
//! `GET /api/config`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::UnknownName;

/// A document family plugins may transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Domain {
    Agent,
    Command,
    Integration,
    Mcp,
    Model,
    Provider,
    Reference,
    Skill,
    Tool,
    Vcs,
    Websearch,
}

impl Domain {
    /// Every domain, in the order used by the compatibility matrix.
    pub const ALL: [Self; 11] = [
        Self::Agent,
        Self::Command,
        Self::Integration,
        Self::Mcp,
        Self::Model,
        Self::Provider,
        Self::Reference,
        Self::Skill,
        Self::Tool,
        Self::Vcs,
        Self::Websearch,
    ];

    /// Wire name, as used in the sidecar `transform` request and inventory.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Command => "command",
            Self::Integration => "integration",
            Self::Mcp => "mcp",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Reference => "reference",
            Self::Skill => "skill",
            Self::Tool => "tool",
            Self::Vcs => "vcs",
            Self::Websearch => "websearch",
        }
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Domain {
    type Err = UnknownName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|domain| domain.as_str() == name)
            .ok_or_else(|| UnknownName {
                kind: "transform domain",
                name: name.to_string(),
            })
    }
}
