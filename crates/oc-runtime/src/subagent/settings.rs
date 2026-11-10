use std::collections::BTreeMap;
use std::time::Duration;

use oc_config::ConfigService;
use oc_llm::Catalog;
use oc_subagents::{Limits, ModelDefaults, Role};
use oc_tools::ToolError;
use oc_types::ModelRef;
use serde_json::{Map, Value};

use crate::runtime::Runtime;

use super::tree::{json_error, optional_str};

/// Resolved `subagents` configuration plus the per-agent overrides the tools
/// need.
#[derive(Debug, Clone)]
pub(crate) struct SubagentSettings {
    pub(super) limits: Limits,
    pub(super) timeout: Duration,
    pub(super) background_default: bool,
    pub(super) digest_model: Option<String>,
    pub(super) siblings_wake: bool,
    pub(super) overrides: BTreeMap<String, AgentOverride>,
}

/// One `subagents.agents.<id>` entry.
#[derive(Debug, Clone, Default)]
pub(super) struct AgentOverride {
    pub(super) model: Option<String>,
    pub(super) role: Option<String>,
    pub(super) allow_spawn: Option<bool>,
    pub(super) max_children: Option<u32>,
}

/// Read the `subagents` config section, honoring the legacy
/// `experimental.subagent_depth` when `subagents.max_depth` is absent.
pub(crate) fn load_settings(config: &ConfigService) -> SubagentSettings {
    let merged = config.merged();
    let section = merged.get("subagents");
    let field = |name: &str| section.and_then(|value| value.get(name));

    let legacy_depth = merged
        .pointer("/experimental/subagent_depth")
        .and_then(Value::as_u64);
    let configured_depth = field("max_depth").and_then(Value::as_u64);
    if legacy_depth.is_some() {
        if configured_depth.is_some() {
            tracing::warn!(
                "both subagents.max_depth and experimental.subagent_depth are set; \
                 subagents.max_depth wins"
            );
        } else {
            tracing::warn!(
                "experimental.subagent_depth is deprecated; set subagents.max_depth instead"
            );
        }
    }
    let max_depth = configured_depth.or(legacy_depth).unwrap_or(3);

    let siblings = field("siblings");
    let sibling_field = |name: &str| siblings.and_then(|value| value.get(name));

    let limits = Limits {
        enabled: field("enabled").and_then(Value::as_bool).unwrap_or(true),
        max_depth: u32::try_from(max_depth).unwrap_or(u32::MAX),
        max_children: u32::try_from(
            field("max_children_per_session")
                .and_then(Value::as_u64)
                .unwrap_or(8),
        )
        .unwrap_or(u32::MAX),
        allow_spawn: field("allow_spawn")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        default_model: field("default_model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        default_agent: field("default_agent")
            .and_then(Value::as_str)
            .unwrap_or("general")
            .to_owned(),
        siblings_enabled: sibling_field("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        allow_parent: sibling_field("allow_parent")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        max_messages_per_min: u32::try_from(
            sibling_field("max_messages_per_min")
                .and_then(Value::as_u64)
                .unwrap_or(20),
        )
        .unwrap_or(u32::MAX),
        max_writes_per_min: u32::try_from(
            sibling_field("max_writes_per_min")
                .and_then(Value::as_u64)
                .unwrap_or(20),
        )
        .unwrap_or(u32::MAX),
    };

    let timeout_seconds = field("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .max(1);
    let mut overrides = BTreeMap::new();
    if let Some(agents) = field("agents").and_then(Value::as_object) {
        for (id, value) in agents {
            overrides.insert(id.clone(), parse_override(value));
        }
    }

    SubagentSettings {
        limits,
        timeout: Duration::from_secs(timeout_seconds),
        background_default: field("background_default")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        digest_model: field("digest_model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        siblings_wake: sibling_field("wake")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        overrides,
    }
}

fn parse_override(value: &Value) -> AgentOverride {
    AgentOverride {
        model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        role: value.get("role").and_then(Value::as_str).map(str::to_owned),
        allow_spawn: value.get("allow_spawn").and_then(Value::as_bool),
        max_children: value
            .get("max_children")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
    }
}

pub(super) fn settings(runtime: &Runtime) -> SubagentSettings {
    load_settings(&runtime.inner.config.config)
}

/// Resolve the active subagent mode (`"complex"` or `"basic"`) for `session_id`
/// from the session or its root session's `metadata.subagentMode` (or
/// `metadata.mode`), falling back to `subagents.mode` in the merged config.
pub(crate) async fn session_subagent_mode(runtime: &Runtime, session_id: &str) -> Option<String> {
    let store = &runtime.inner.config.store;
    let root_id = store
        .session_node(session_id)
        .await
        .ok()
        .flatten()
        .map_or_else(|| session_id.to_owned(), |node| node.root_id);
    for id in [session_id, root_id.as_str()] {
        if let Ok(Some(session)) = store.get_session(id).await
            && let Some(mode) = session
                .pointer("/metadata/subagentMode")
                .or_else(|| session.pointer("/metadata/mode"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|mode| !mode.is_empty())
        {
            return Some(mode.to_ascii_lowercase());
        }
    }
    runtime
        .inner
        .config
        .config
        .merged()
        .pointer("/subagents/mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Resolve [`SubagentSettings`] for `session_id`, applying `/complex` (3-level
/// hierarchy) or `/basic` (single-level hierarchy) overrides from the root
/// session's metadata.
pub(super) async fn settings_for_session(runtime: &Runtime, session_id: &str) -> SubagentSettings {
    let mut resolved = settings(runtime);
    match session_subagent_mode(runtime, session_id).await.as_deref() {
        Some("complex") => {
            resolved.limits.max_depth = 3;
            resolved.limits.max_children = 64;
            resolved.limits.allow_spawn = true;
            resolved.limits.siblings_enabled = true;
            if resolved.timeout < Duration::from_secs(3600) {
                resolved.timeout = Duration::from_secs(3600);
            }
        }
        Some("basic") => {
            resolved.limits.max_depth = 1;
        }
        _ => {}
    }
    resolved
}

pub(super) fn available_agents(config: &ConfigService) -> Vec<String> {
    config
        .agents()
        .into_iter()
        .filter(|agent| {
            let mode = agent
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("primary");
            let hidden = agent
                .get("hidden")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            !hidden && (mode == "subagent" || mode == "all")
        })
        .filter_map(|agent| agent.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

pub(super) fn available_models(catalog: &Catalog) -> Vec<String> {
    let mut models = Vec::new();
    for model in catalog.models() {
        let chat = model.kind.as_deref().is_none_or(|kind| kind == "chat");
        if !chat || !model.enabled {
            continue;
        }
        models.push(format!("{}/{}", model.provider_id, model.model_id));
        for variant in &model.variants {
            models.push(format!(
                "{}/{}#{}",
                model.provider_id, model.model_id, variant.id
            ));
        }
    }
    models
}

pub(super) fn model_defaults(config: &ConfigService, settings: &SubagentSettings) -> ModelDefaults {
    let mut defaults = ModelDefaults::new();
    for agent in config.agents() {
        let Some(id) = agent.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(model) = agent.get("model").and_then(crate::prompt::model_ref) {
            defaults
                .agent_models
                .insert(id.to_owned(), model.to_reference());
        }
    }
    for (id, over) in &settings.overrides {
        if let Some(model) = &over.model {
            defaults.policy_models.insert(id.clone(), model.clone());
        }
    }
    if let Some(model) = &settings.limits.default_model {
        defaults.default_model = Some(model.clone());
    }
    defaults
}

pub(super) fn requested_role(input: &Value, settings: &SubagentSettings, agent: &str) -> Role {
    optional_str(input, "role")
        .or_else(|| {
            settings
                .overrides
                .get(agent)
                .and_then(|over| over.role.clone())
        })
        .map_or(Role::Subagent, |role| match role.as_str() {
            "coordinator" => Role::Coordinator,
            _ => Role::Subagent,
        })
}

pub(super) fn model_value(reference: &str, catalog: &Catalog) -> Result<Value, ToolError> {
    let resolved = catalog
        .resolve(reference)
        .map_err(|error| json_error(&error))?;
    let model_ref = ModelRef {
        id: resolved.model.model_id.clone(),
        provider_id: resolved.model.provider_id.clone(),
        variant: resolved.variant.as_ref().map(|variant| variant.id.clone()),
    };
    let mut value = Map::new();
    value.insert("id".to_owned(), Value::String(model_ref.id));
    value.insert(
        "providerID".to_owned(),
        Value::String(model_ref.provider_id),
    );
    if let Some(variant) = model_ref.variant {
        value.insert("variant".to_owned(), Value::String(variant));
    }
    Ok(Value::Object(value))
}
