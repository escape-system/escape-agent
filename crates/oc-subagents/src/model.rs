//! Model resolution for a child session.
//!
//! Order (first hit wins, per `docs/05-subagents.md`):
//! call model → `agent.<id>.model` → `subagents.agents.<id>.model` →
//! `subagents.default_model` → parent session model.
//!
//! A candidate that is malformed or absent from the catalog is an error, not a
//! fall-through: a call asking for an unavailable model must fail loudly.

use std::collections::BTreeMap;

/// Where the resolved model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelSource {
    Call,
    Agent,
    Default,
    Parent,
}

impl ModelSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Agent => "agent",
            Self::Default => "default",
            Self::Parent => "parent",
        }
    }
}

impl std::fmt::Display for ModelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the candidate chain, kept for error reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCandidate {
    pub reference: String,
    pub source: ModelSource,
}

impl ModelCandidate {
    #[must_use]
    pub fn new(reference: impl Into<String>, source: ModelSource) -> Self {
        Self {
            reference: reference.into(),
            source,
        }
    }
}

/// The resolved model and its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub reference: String,
    pub source: ModelSource,
}

/// Already-resolved config lookups. The runtime builds these from
/// `ConfigService`; the policy crate never reads config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelDefaults {
    /// `agent.<id>.model`.
    pub agent_models: BTreeMap<String, String>,
    /// `subagents.agents.<id>.model`.
    pub policy_models: BTreeMap<String, String>,
    /// `subagents.default_model`.
    pub default_model: Option<String>,
}

impl ModelDefaults {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_agent_model(mut self, agent: impl Into<String>, model: impl Into<String>) -> Self {
        self.agent_models.insert(agent.into(), model.into());
        self
    }

    #[must_use]
    pub fn with_policy_model(mut self, agent: impl Into<String>, model: impl Into<String>) -> Self {
        self.policy_models.insert(agent.into(), model.into());
        self
    }

    #[must_use]
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }
}

/// Resolve the child's model reference.
///
/// `available` is the catalog's exact reference list (`provider/id[#variant]`).
/// An unknown variant therefore fails membership and surfaces with the full
/// candidate chain.
pub fn resolve_model(
    call: Option<&str>,
    agent: Option<&str>,
    defaults: ModelDefaults,
    parent: &str,
    available: &[String],
) -> Result<ModelChoice, crate::ModelError> {
    let ModelDefaults {
        agent_models,
        policy_models,
        default_model,
    } = defaults;

    let mut chain = Vec::new();
    if let Some(reference) = call {
        chain.push(ModelCandidate::new(reference, ModelSource::Call));
    }
    if let Some(agent) = agent {
        if let Some(reference) = agent_models.get(agent) {
            chain.push(ModelCandidate::new(reference, ModelSource::Agent));
        }
        if let Some(reference) = policy_models.get(agent) {
            chain.push(ModelCandidate::new(reference, ModelSource::Agent));
        }
    }
    if let Some(reference) = default_model {
        chain.push(ModelCandidate::new(reference, ModelSource::Default));
    }
    if !parent.is_empty() {
        chain.push(ModelCandidate::new(parent, ModelSource::Parent));
    }

    let Some(chosen) = chain.first() else {
        return Err(crate::ModelError::NoCandidate);
    };
    if !well_formed(&chosen.reference) {
        return Err(crate::ModelError::InvalidReference {
            reference: chosen.reference.clone(),
            origin: chosen.source.as_str(),
            chain,
        });
    }
    if !available.iter().any(|model| model == &chosen.reference) {
        return Err(crate::ModelError::Unavailable {
            reference: chosen.reference.clone(),
            origin: chosen.source.as_str(),
            chain,
        });
    }
    Ok(ModelChoice {
        reference: chosen.reference.clone(),
        source: chosen.source,
    })
}

/// `provider/id` or `provider/id#variant`.
fn well_formed(reference: &str) -> bool {
    let (base, variant) = match reference.split_once('#') {
        Some((base, variant)) => (base, Some(variant)),
        None => (reference, None),
    };
    if variant.is_some_and(|variant| variant.is_empty() || variant.contains('#')) {
        return false;
    }
    match base.split_once('/') {
        Some((provider, id)) => !provider.is_empty() && !id.is_empty(),
        None => false,
    }
}
