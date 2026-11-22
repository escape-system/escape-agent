use std::collections::BTreeMap;
use std::time::Duration;

use oc_config::ConfigService;
use oc_llm::{Catalog, FailureKind, ModelInfo, SharedLlmClient};
use oc_subagents::{Limits, ModelDefaults, Role};
use oc_tools::ToolError;
use oc_types::ModelRef;
use serde_json::{Map, Value};

use crate::runtime::Runtime;

use super::tree::{json_error, optional_str};

/// Resolved `subagents` configuration plus the per-agent overrides the tools
/// need.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentSettings {
    pub(super) limits: Limits,
    pub(super) timeout: Duration,
    pub(super) background_default: bool,
    pub(super) digest_model: Option<String>,
    pub(super) siblings_wake: bool,
    pub(super) overrides: BTreeMap<String, AgentOverride>,
    /// Cap on the child's final text embedded in the parent report.
    // 0 = unlimited (default)
    pub report_char_limit: usize,
    /// `models` control-plane policy (chains, quota, fallback).
    pub(crate) models: ModelsPolicy,
}

/// `models` control-plane policy ([`docs/14-config-permissions.md`]).
///
/// Parsed per call — never cached — because `PUT /api/config/models` writes the
/// shared config cell in place and every spawn must read the current document. A
/// malformed value is recorded in [`ModelsPolicy::error`] instead of panicking;
/// the spawn chooser turns it into a typed tool error before any session row is
/// written.
#[derive(Debug, Clone, Default)]
pub struct ModelsPolicy {
    /// `models.chains.<agent>`.
    pub(crate) chains: BTreeMap<String, Vec<String>>,
    /// `models.chain`.
    pub(crate) chain: Vec<String>,
    /// `models.quota`; `None` reproduces the pre-quota behavior exactly.
    pub(crate) quota: Option<QuotaPolicy>,
    /// `models.fallback`.
    pub(crate) fallback: FallbackPolicy,
    /// First malformed `models` value, surfaced by the spawn chooser.
    pub(crate) error: Option<ModelsConfigError>,
}

impl ModelsPolicy {
    /// Ordered chain for `agent`: `models.chains.<agent>` wins over
    /// `models.chain`.
    #[must_use]
    pub fn chain_for(&self, agent: &str) -> &[String] {
        self.chains
            .get(agent)
            .map_or(self.chain.as_slice(), Vec::as_slice)
    }

    /// `models.fallback` as parsed, for callers that need the hop budget or the
    /// raw configured list.
    #[must_use]
    pub fn fallback(&self) -> &FallbackPolicy {
        &self.fallback
    }
}

/// `models.fallback`: which failure classes advance the chain and how far.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FallbackPolicy {
    /// Failure kinds that advance the chain (`models.fallback.on`).
    pub(crate) on: Vec<FailureKind>,
    /// Maximum model switches per turn (`models.fallback.max_hops`).
    pub(crate) max_hops: u32,
    /// True when `models.fallback` was present (an explicit empty `on` is a
    /// deliberate "never fall back").
    pub(crate) configured: bool,
}

impl FallbackPolicy {
    /// Automatic mid-turn model fallback is disabled: callers rotate keys on
    /// the same model via [`oc_llm::KeyPool`], and model selection belongs to
    /// the caller/main agent rather than a silent fallback hop.
    #[must_use]
    pub fn allows(&self, _kind: FailureKind) -> bool {
        false
    }

    /// The classes an explicit `models.fallback.on` switched off.
    ///
    /// Only the opt-in classes (`empty_response`, `transport`) can be disabled:
    /// [`FallbackPolicy::allows`] advances on the recovery classes regardless, so
    /// reporting them here would be a lie. Empty when `models.fallback` is absent
    /// (the default set is complete) — the Models tab renders a warning from this
    /// list instead of adding a new route.
    #[must_use]
    pub fn disabled_by_config(&self) -> Vec<&'static str> {
        if !self.configured {
            return Vec::new();
        }
        retryable_kinds()
            .into_iter()
            .filter(|kind| !recovery_kinds().contains(kind))
            .filter(|kind| !self.on.contains(kind))
            .map(FailureKind::as_str)
            .collect()
    }

    /// The effective advance set: the configured `on` plus the recovery classes
    /// that cannot be switched off.
    ///
    /// This is what the UI and the docs report; [`FallbackPolicy::on`] stays the
    /// raw configured list so a round trip through `PUT /api/config/models`
    /// never rewrites the document behind the user's back.
    #[must_use]
    pub fn effective_on(&self) -> Vec<FailureKind> {
        let mut kinds = self.on.clone();
        for kind in recovery_kinds() {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        kinds
    }
}

/// Classes that advance the chain in raw config compatibility checks.
pub(crate) fn recovery_kinds() -> [FailureKind; 2] {
    [FailureKind::Quota, FailureKind::RateLimit]
}

/// The four retryable failure classes, in event-payload order.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn retryable_kinds() -> Vec<FailureKind> {
    vec![
        FailureKind::EmptyResponse,
        FailureKind::Transport,
        FailureKind::RateLimit,
        FailureKind::Quota,
    ]
}

/// Every failure class `models.fallback.on` may name: the retryable set plus the
/// unconditionally-advancing [`FailureKind::ModelUnavailable`].
fn accepted_fallback_kinds() -> Vec<FailureKind> {
    let mut kinds = retryable_kinds();
    kinds.push(FailureKind::ModelUnavailable);
    kinds
}

/// `models.quota`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuotaPolicy {
    /// Share of a model's window kept in reserve (`models.quota.reserve_percent`).
    pub(crate) reserve_percent: u8,
    /// Rolling window the usage rollup is compared against.
    pub(crate) window_hours: u32,
    /// Applied to every model without an explicit entry.
    pub(crate) default: ModelBudget,
    /// Per-model budgets, keyed by variant-insensitive `provider/id`.
    pub(crate) models: BTreeMap<String, ModelBudget>,
}

impl QuotaPolicy {
    /// Effective budget for `reference`: the per-model entry wins over
    /// `models.quota.default`.
    pub(crate) fn budget_for(&self, reference: &str) -> Option<&ModelBudget> {
        if let Some(budget) = self.models.get(&model_key(reference)) {
            return Some(budget);
        }
        self.default.is_limited().then_some(&self.default)
    }

    /// Fraction of the budget that may be spent before the model is treated as
    /// exhausted: `(100 - reserve_percent) / 100`.
    pub(crate) fn usable_fraction(&self) -> f64 {
        f64::from(100_u8.saturating_sub(self.reserve_percent.min(100))) / 100.0
    }
}

/// One `{max_cost_usd?, max_requests?}` budget.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelBudget {
    pub(crate) max_cost_usd: Option<f64>,
    pub(crate) max_requests: Option<u64>,
}

impl ModelBudget {
    pub(crate) fn is_limited(&self) -> bool {
        self.max_cost_usd.is_some() || self.max_requests.is_some()
    }
}

/// Variant-insensitive budget key: quota is tracked per `provider/id`.
pub(crate) fn model_key(reference: &str) -> String {
    reference
        .split_once('#')
        .map_or(reference, |(base, _)| base)
        .to_owned()
}

/// A malformed `models` value. Never panics: the parse functions return this
/// and the spawn chooser fails the tool call with it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ModelsConfigError {
    #[error("models.{key} must be an array of model references")]
    ChainShape { key: String },
    #[error("models.chains must be an object of agent -> array of model references")]
    ChainsShape,
    #[error("models.{key}[{index}] must be a non-empty model reference string")]
    ChainEntry { key: String, index: usize },
    #[error(
        "models.fallback.on must be an array of failure kinds; {value:?} is not one of {allowed}"
    )]
    FallbackOn { value: String, allowed: String },
    #[error("models.fallback.max_hops must be an integer in 0..=5, got {value}")]
    FallbackMaxHops { value: String },
    #[error("models.quota.reserve_percent must be an integer in 0..=100, got {value}")]
    ReservePercent { value: String },
    #[error("models.quota.window_hours must be an integer in 1..=720, got {value}")]
    WindowHours { value: String },
    #[error("models.quota.{scope}.max_cost_usd must be a non-negative number, got {value}")]
    MaxCost { scope: String, value: String },
    #[error("models.quota.{scope}.max_requests must be a non-negative integer, got {value}")]
    MaxRequests { scope: String, value: String },
    #[error("models.{scope} must be an object with max_cost_usd/max_requests")]
    BudgetShape { scope: String },
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
        // 0 = unlimited (default): nothing is dropped unless configured.
        report_char_limit: field("report_char_limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0),
        models: parse_models_policy(&config.models_section()),
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

/// Parse the `models` control-plane section into [`ModelsPolicy`].
///
/// Every malformed value becomes the first [`ModelsConfigError`] and parsing
/// stops there: a half-read quota is worse than a loud refusal, and the spawn
/// chooser must not decide on a partially applied policy.
fn parse_models_policy(section: &Value) -> ModelsPolicy {
    let mut policy = ModelsPolicy::default();
    match parse_models_policy_inner(section, &mut policy) {
        Ok(()) => policy,
        Err(error) => {
            policy.error = Some(error);
            policy
        }
    }
}

/// Parse the `models` control-plane section for crate-level consumers (the turn
/// fallback). Same parser the spawn chooser uses, so one document cannot mean
/// two different policies.
#[must_use]
pub fn models_policy(config: &ConfigService) -> ModelsPolicy {
    parse_models_policy(&config.models_section())
}

fn parse_models_policy_inner(
    section: &Value,
    policy: &mut ModelsPolicy,
) -> Result<(), ModelsConfigError> {
    let object = section.as_object().cloned().unwrap_or_default();

    if let Some(value) = object.get("chain").filter(|value| !value.is_null()) {
        policy.chain = parse_chain(value, "chain")?;
    }
    if let Some(value) = object.get("chains").filter(|value| !value.is_null()) {
        let chains = value.as_object().ok_or(ModelsConfigError::ChainsShape)?;
        for (agent, entries) in chains {
            if entries.is_null() {
                continue;
            }
            let key = format!("chains.{agent}");
            policy
                .chains
                .insert(agent.clone(), parse_chain(entries, &key)?);
        }
    }
    policy.fallback = parse_fallback(object.get("fallback"))?;
    if let Some(value) = object.get("quota").filter(|value| !value.is_null()) {
        policy.quota = Some(parse_quota(value)?);
    }
    Ok(())
}

/// `models.chain` / `models.chains.<agent>`: an array of model references.
fn parse_chain(value: &Value, key: &str) -> Result<Vec<String>, ModelsConfigError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    let entries = value
        .as_array()
        .ok_or_else(|| ModelsConfigError::ChainShape {
            key: key.to_owned(),
        })?;
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let reference = entry
            .as_str()
            .map(str::trim)
            .filter(|reference| !reference.is_empty())
            .ok_or_else(|| ModelsConfigError::ChainEntry {
                key: key.to_owned(),
                index,
            })?;
        out.push(reference.to_owned());
    }
    Ok(out)
}

/// `models.fallback` — failure classes that advance the chain and hop cap.
fn parse_fallback(value: Option<&Value>) -> Result<FallbackPolicy, ModelsConfigError> {
    let mut policy = FallbackPolicy::default();
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(policy);
    };
    policy.configured = true;
    let object = value.as_object().cloned().unwrap_or_default();
    if let Some(on) = object.get("on").filter(|value| !value.is_null()) {
        let entries = on.as_array().ok_or_else(|| ModelsConfigError::FallbackOn {
            value: on.to_string(),
            allowed: kind_list(),
        })?;
        let mut kinds = Vec::with_capacity(entries.len());
        for entry in entries {
            let text = entry.as_str().map(str::trim).unwrap_or_default();
            let kind = accepted_fallback_kinds()
                .into_iter()
                .find(|kind| kind.as_str() == text)
                .ok_or_else(|| ModelsConfigError::FallbackOn {
                    value: text.to_owned(),
                    allowed: kind_list(),
                })?;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        policy.on = kinds;
    }
    if let Some(hops) = object.get("max_hops").filter(|value| !value.is_null()) {
        let raw = as_count(hops).ok_or_else(|| ModelsConfigError::FallbackMaxHops {
            value: hops.to_string(),
        })?;
        if raw > 5 {
            return Err(ModelsConfigError::FallbackMaxHops {
                value: hops.to_string(),
            });
        }
        policy.max_hops = u32::try_from(raw).unwrap_or(u32::MAX);
    }
    Ok(policy)
}

/// Read a JSON number as a non-negative integer.
///
/// JSON carries no int/float distinction, and `PUT /api/config/models` used to
/// persist `reserve_percent` as `10.0`; a float without a fractional part is the
/// same quota value and must parse. A real fraction (`10.5`), a negative number
/// or a non-number still becomes a typed error, exactly like the previous
/// `as_u64` reader.
// `fract() == 0.0` and the sign check make the cast exact for every value these
// fields can hold; an out-of-range float saturates instead of wrapping.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_count(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    let number = value.as_f64()?;
    if !number.is_finite() || number < 0.0 || number.fract() != 0.0 {
        return None;
    }
    Some(number as u64)
}

/// `models.quota` — reserve share, rolling window and per-model budgets.
fn parse_quota(value: &Value) -> Result<QuotaPolicy, ModelsConfigError> {
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| ModelsConfigError::BudgetShape {
            scope: "quota".to_owned(),
        })?;
    let reserve_percent = match object
        .get("reserve_percent")
        .filter(|value| !value.is_null())
    {
        Some(raw) => {
            let number = as_count(raw).ok_or_else(|| ModelsConfigError::ReservePercent {
                value: raw.to_string(),
            })?;
            // Clamp instead of rejecting: 100 is "spend nothing", 0 is "no reserve".
            u8::try_from(number.min(100)).unwrap_or(100)
        }
        None => 0,
    };
    let window_hours = match object.get("window_hours").filter(|value| !value.is_null()) {
        Some(raw) => {
            let number = as_count(raw).ok_or_else(|| ModelsConfigError::WindowHours {
                value: raw.to_string(),
            })?;
            // Clamp the documented 1..=720 range rather than refusing a config
            // that only overshoots it.
            u32::try_from(number.clamp(1, 720)).unwrap_or(720)
        }
        None => 5,
    };
    let default = match object.get("default").filter(|value| !value.is_null()) {
        Some(raw) => parse_budget(raw, "default")?,
        None => ModelBudget::default(),
    };
    let mut models = BTreeMap::new();
    if let Some(entries) = object.get("models").filter(|value| !value.is_null()) {
        let entries = entries
            .as_object()
            .ok_or_else(|| ModelsConfigError::BudgetShape {
                scope: "models".to_owned(),
            })?;
        for (reference, raw) in entries {
            let key = model_key(reference);
            let scope = format!("models.{reference}");
            models.insert(key, parse_budget(raw, &scope)?);
        }
    }
    Ok(QuotaPolicy {
        reserve_percent,
        window_hours,
        default,
        models,
    })
}

fn parse_budget(value: &Value, scope: &str) -> Result<ModelBudget, ModelsConfigError> {
    let object = value
        .as_object()
        .ok_or_else(|| ModelsConfigError::BudgetShape {
            scope: scope.to_owned(),
        })?;
    let max_cost_usd = match object.get("max_cost_usd").filter(|value| !value.is_null()) {
        Some(raw) => {
            let number = raw.as_f64().ok_or_else(|| ModelsConfigError::MaxCost {
                scope: scope.to_owned(),
                value: raw.to_string(),
            })?;
            if !number.is_finite() || number < 0.0 {
                return Err(ModelsConfigError::MaxCost {
                    scope: scope.to_owned(),
                    value: raw.to_string(),
                });
            }
            Some(number)
        }
        None => None,
    };
    let max_requests = match object.get("max_requests").filter(|value| !value.is_null()) {
        Some(raw) => Some(as_count(raw).ok_or_else(|| ModelsConfigError::MaxRequests {
            scope: scope.to_owned(),
            value: raw.to_string(),
        })?),
        None => None,
    };
    Ok(ModelBudget {
        max_cost_usd,
        max_requests,
    })
}

fn kind_list() -> String {
    accepted_fallback_kinds()
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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
pub(crate) async fn settings_for_session(runtime: &Runtime, session_id: &str) -> SubagentSettings {
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

pub(super) fn primary_only_agents(config: &ConfigService) -> Vec<String> {
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
            hidden || (mode != "subagent" && mode != "all")
        })
        .filter_map(|agent| agent.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

pub(super) fn available_models(catalog: &Catalog, client: &SharedLlmClient) -> Vec<String> {
    let mut all_models = catalog.models();
    for extra in client.dynamic_models() {
        if let Some(pos) = all_models
            .iter()
            .position(|m| m.provider_id == extra.provider_id && m.id == extra.id)
        {
            all_models[pos] = extra;
        } else {
            all_models.push(extra);
        }
    }
    available_models_from_slice(&all_models)
}

fn available_models_from_slice(source: &[ModelInfo]) -> Vec<String> {
    let mut models = Vec::new();
    for model in source {
        let chat = model.kind.as_deref().is_none_or(|kind| kind == "chat");
        if !chat || !model.enabled {
            continue;
        }
        models.push(format!("{}/{}", model.provider_id, model.id));
        for variant in &model.variants {
            models.push(format!("{}/{}#{}", model.provider_id, model.id, variant.id));
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

pub(super) fn model_value(
    reference: &str,
    catalog: &Catalog,
    client: &SharedLlmClient,
) -> Result<Value, ToolError> {
    let resolved = client
        .resolve_model(catalog, reference)
        .map_err(|error| json_error(&error))?;
    let model_ref = ModelRef {
        id: resolved.model.id.clone(),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use oc_llm::FailureKind;
    use serde_json::json;

    use super::{
        FallbackPolicy, ModelBudget, ModelsConfigError, ModelsPolicy, QuotaPolicy, model_key,
        parse_models_policy,
    };

    fn quota_of(parsed: &ModelsPolicy) -> QuotaPolicy {
        parsed.quota.clone().expect("quota parsed")
    }

    #[test]
    fn models_policy_parses_a_valid_document() {
        let parsed = parse_models_policy(&json!({
            "chain": ["google/gemini-3-pro", "opencode-go/glm-5.3-flash"],
            "chains": {"coordinator": ["google/gemini-3-pro"]},
            "fallback": {"on": ["empty_response", "quota"], "max_hops": 1},
            "quota": {
                "reserve_percent": 10,
                "window_hours": 5,
                "default": {"max_cost_usd": 5.0, "max_requests": 100},
                "models": {"google/gemini-3-pro#thinking": {"max_cost_usd": 2.0}},
            },
        }));
        assert_eq!(parsed.error, None);
        assert_eq!(parsed.chain.len(), 2);
        assert_eq!(
            parsed.chains.get("coordinator"),
            Some(&vec!["google/gemini-3-pro".to_owned()])
        );
        assert_eq!(
            parsed.fallback.on,
            vec![FailureKind::EmptyResponse, FailureKind::Quota]
        );
        assert_eq!(parsed.fallback.max_hops, 1);
        assert!(parsed.fallback.configured);

        let quota = quota_of(&parsed);
        assert_eq!(quota.reserve_percent, 10);
        assert_eq!(quota.window_hours, 5);
        assert_eq!(quota.default.max_requests, Some(100));
        assert!(
            quota
                .default
                .max_cost_usd
                .is_some_and(|max| (max - 5.0).abs() < 1e-9)
        );
        // The variant suffix is stripped, so a variant shares its base budget.
        let per_model = quota.models.get("google/gemini-3-pro").expect("per-model");
        assert_eq!(per_model.max_requests, None);
        assert!(
            per_model
                .max_cost_usd
                .is_some_and(|max| (max - 2.0).abs() < 1e-9)
        );
    }

    #[test]
    fn models_policy_absent_section_is_the_default_policy() {
        // `models_section()` always hands the parser an object; an absent key
        // therefore reaches it as `{}`.
        let parsed = parse_models_policy(&json!({}));
        assert_eq!(parsed.error, None);
        assert!(parsed.chain.is_empty());
        assert!(parsed.chains.is_empty());
        assert!(parsed.quota.is_none());
        let expected = FallbackPolicy {
            on: Vec::new(),
            max_hops: 0,
            configured: false,
        };
        assert_eq!(parsed.fallback, expected);
    }

    #[test]
    fn malformed_models_values_become_typed_errors() {
        let cases = [
            (
                json!({"quota": {"reserve_percent": "ten"}}),
                "ReservePercent",
            ),
            (json!({"quota": {"window_hours": -5}}), "WindowHours"),
            (json!({"quota": []}), "BudgetShape"),
            (
                json!({"quota": {"default": {"max_cost_usd": -1}}}),
                "MaxCost",
            ),
            (
                json!({"quota": {"default": {"max_requests": "many"}}}),
                "MaxRequests",
            ),
            (json!({"fallback": {"max_hops": 9}}), "FallbackMaxHops"),
            (json!({"fallback": {"on": ["auth"]}}), "FallbackOn"),
            (json!({"chain": "google/gemini-3-pro"}), "ChainShape"),
            (json!({"chains": []}), "ChainsShape"),
            (json!({"chain": [""]}), "ChainEntry"),
        ];
        for (document, expected) in cases {
            let error = parse_models_policy(&document)
                .error
                .unwrap_or_else(|| panic!("{document} must not parse silently"));
            let name = match &error {
                ModelsConfigError::ReservePercent { .. } => "ReservePercent",
                ModelsConfigError::WindowHours { .. } => "WindowHours",
                ModelsConfigError::BudgetShape { .. } => "BudgetShape",
                ModelsConfigError::MaxCost { .. } => "MaxCost",
                ModelsConfigError::MaxRequests { .. } => "MaxRequests",
                ModelsConfigError::FallbackMaxHops { .. } => "FallbackMaxHops",
                ModelsConfigError::FallbackOn { .. } => "FallbackOn",
                ModelsConfigError::ChainShape { .. } => "ChainShape",
                ModelsConfigError::ChainsShape => "ChainsShape",
                ModelsConfigError::ChainEntry { .. } => "ChainEntry",
            };
            assert_eq!(name, expected, "{document}: {error}");
            // Typed, displayed and never a panic: the message names the field.
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn out_of_range_quota_integers_clamp_rather_than_failing() {
        // Pins the implemented behavior (`parse_quota` clamps on purpose): the
        // typed error is reserved for non-integers. Asserting the clamp keeps a
        // future "reject out-of-range" change honest instead of silent.
        let parsed = parse_models_policy(&json!({
            "quota": {"reserve_percent": 150, "window_hours": 0},
        }));
        assert_eq!(parsed.error, None);
        let quota = quota_of(&parsed);
        assert_eq!(quota.reserve_percent, 100);
        assert_eq!(quota.window_hours, 1);

        let wide = quota_of(&parse_models_policy(&json!({
            "quota": {"window_hours": 1000},
        })));
        assert_eq!(wide.window_hours, 720);
    }

    #[test]
    fn integral_floats_parse_as_quota_integers() {
        // Regression: `PUT /api/config/models` used to persist
        // `"reserve_percent": 10.0`. `as_u64` returned `None` for that float,
        // so the parser emitted `ReservePercent` and every subagent spawn hard
        // failed. JSON numbers are untyped: an integral float is the same value.
        let parsed = parse_models_policy(&json!({
            "quota": {
                "reserve_percent": 10.0,
                "window_hours": 5.0,
                "default": {"max_cost_usd": 5.0, "max_requests": 100.0},
            },
            "fallback": {"max_hops": 1.0},
        }));
        assert_eq!(parsed.error, None, "{:?}", parsed.error);
        let quota = quota_of(&parsed);
        assert_eq!(quota.reserve_percent, 10);
        assert_eq!(quota.window_hours, 5);
        assert_eq!(quota.default.max_requests, Some(100));
        assert_eq!(parsed.fallback.max_hops, 1);

        // The integer spelling is unchanged.
        let ints = quota_of(&parse_models_policy(&json!({
            "quota": {"reserve_percent": 10, "window_hours": 5},
        })));
        assert_eq!(ints.reserve_percent, 10);
        assert_eq!(ints.window_hours, 5);

        // A zero reserve survives either spelling.
        let zero = quota_of(&parse_models_policy(&json!({
            "quota": {"reserve_percent": 0.0, "window_hours": 0.0},
        })));
        assert_eq!(zero.reserve_percent, 0);
        assert_eq!(zero.window_hours, 1);
    }

    #[test]
    fn integral_float_quota_values_still_clamp() {
        let high = quota_of(&parse_models_policy(&json!({
            "quota": {"reserve_percent": 1000.0, "window_hours": 1000.0},
        })));
        assert_eq!(high.reserve_percent, 100);
        assert_eq!(high.window_hours, 720);
        // Same clamp as the integer spelling.
        let high_int = quota_of(&parse_models_policy(&json!({
            "quota": {"reserve_percent": 1000, "window_hours": 1000},
        })));
        assert_eq!(high_int.reserve_percent, high.reserve_percent);
        assert_eq!(high_int.window_hours, high.window_hours);
    }

    #[test]
    fn genuine_non_integers_still_error_after_the_float_leniency() {
        let cases = [
            (
                json!({"quota": {"reserve_percent": "ten"}}),
                "ReservePercent",
            ),
            (json!({"quota": {"reserve_percent": -5}}), "ReservePercent"),
            (
                json!({"quota": {"reserve_percent": 10.5}}),
                "ReservePercent",
            ),
            (
                json!({"quota": {"reserve_percent": true}}),
                "ReservePercent",
            ),
            (json!({"quota": {"window_hours": 5.5}}), "WindowHours"),
            (
                json!({"quota": {"default": {"max_requests": 1.5}}}),
                "MaxRequests",
            ),
            (json!({"fallback": {"max_hops": 1.5}}), "FallbackMaxHops"),
        ];
        for (document, expected) in cases {
            let error = parse_models_policy(&document)
                .error
                .unwrap_or_else(|| panic!("{document} must not parse silently"));
            let name = match &error {
                ModelsConfigError::ReservePercent { .. } => "ReservePercent",
                ModelsConfigError::WindowHours { .. } => "WindowHours",
                ModelsConfigError::MaxRequests { .. } => "MaxRequests",
                ModelsConfigError::FallbackMaxHops { .. } => "FallbackMaxHops",
                other => panic!("{document}: unexpected {other:?}"),
            };
            assert_eq!(name, expected, "{document}: {error}");
        }
    }

    #[test]
    fn usable_fraction_is_the_share_left_after_the_reserve() {
        let fraction = |reserve: u8| {
            QuotaPolicy {
                reserve_percent: reserve,
                window_hours: 5,
                default: ModelBudget::default(),
                models: BTreeMap::new(),
            }
            .usable_fraction()
        };
        assert!((fraction(0) - 1.0).abs() < f64::EPSILON);
        assert!((fraction(10) - 0.9).abs() < 1e-9);
        assert!((fraction(50) - 0.5).abs() < f64::EPSILON);
        assert!((fraction(100) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_is_limited_only_when_a_cap_is_set() {
        assert!(!ModelBudget::default().is_limited());
        // A zero cap is still a cap: the model is exhausted from the start.
        assert!(
            ModelBudget {
                max_cost_usd: Some(0.0),
                max_requests: None,
            }
            .is_limited()
        );
        assert!(
            ModelBudget {
                max_cost_usd: None,
                max_requests: Some(1),
            }
            .is_limited()
        );
    }

    #[test]
    fn budget_for_prefers_the_per_model_entry_over_the_default() {
        let quota = QuotaPolicy {
            reserve_percent: 10,
            window_hours: 5,
            default: ModelBudget {
                max_cost_usd: Some(5.0),
                max_requests: Some(100),
            },
            models: BTreeMap::from([(
                "google/gemini-3-pro".to_owned(),
                ModelBudget {
                    max_cost_usd: Some(2.0),
                    max_requests: None,
                },
            )]),
        };
        let per_model = quota
            .budget_for("google/gemini-3-pro#thinking")
            .expect("per-model entry");
        assert_eq!(per_model.max_requests, None);
        let default = quota
            .budget_for("opencode-go/glm-5.3-flash")
            .expect("default entry");
        assert_eq!(default.max_requests, Some(100));

        // An unlimited default denies nothing: no budget means no quota.
        let unlimited = QuotaPolicy {
            reserve_percent: 10,
            window_hours: 5,
            default: ModelBudget::default(),
            models: BTreeMap::new(),
        };
        assert!(unlimited.budget_for("any/model").is_none());
    }

    #[test]
    fn model_key_strips_only_the_variant_suffix() {
        assert_eq!(
            model_key("google/gemini-3-pro#thinking"),
            "google/gemini-3-pro"
        );
        assert_eq!(model_key("google/gemini-3-pro"), "google/gemini-3-pro");
        assert_eq!(model_key("#weird"), "");
    }

    #[test]
    fn chain_for_selects_the_agent_chain_and_is_empty_without_config() {
        // The empty slice is the predicate `prepare_spawn_context` uses to take
        // the legacy `resolve_model` path (no filter, no quota reads).
        let empty = parse_models_policy(&json!({}));
        assert!(empty.chain_for("build").is_empty());

        let parsed = parse_models_policy(&json!({
            "chain": ["opencode-go/glm-5.3-flash"],
            "chains": {"coordinator": ["google/gemini-3-pro"]},
        }));
        assert_eq!(
            parsed.chain_for("coordinator"),
            ["google/gemini-3-pro".to_owned()].as_slice()
        );
        assert_eq!(
            parsed.chain_for("build"),
            ["opencode-go/glm-5.3-flash".to_owned()].as_slice()
        );
    }

    #[test]
    fn fallback_allows_is_always_false_while_config_tracking_stays_honest() {
        let parsed = parse_models_policy(&json!({
            "fallback": {"on": ["empty_response", "transport"], "max_hops": 2}
        }));
        assert_eq!(parsed.error, None);
        assert!(parsed.fallback.configured);
        assert_eq!(
            parsed.fallback.on,
            vec![FailureKind::EmptyResponse, FailureKind::Transport]
        );
        for kind in [
            FailureKind::Quota,
            FailureKind::RateLimit,
            FailureKind::ModelUnavailable,
            FailureKind::EmptyResponse,
            FailureKind::Transport,
            FailureKind::Auth,
            FailureKind::InvalidRequest,
        ] {
            assert!(!parsed.fallback.allows(kind), "{kind:?}");
        }
        assert!(parsed.fallback.disabled_by_config().is_empty());
    }

    #[test]
    fn disabled_by_config_and_effective_on_report_raw_config_compat() {
        let empty = parse_models_policy(&json!({"fallback": {"on": []}}));
        assert_eq!(empty.error, None);
        assert!(!empty.fallback.allows(FailureKind::Quota));
        assert_eq!(
            empty.fallback.disabled_by_config(),
            vec!["empty_response", "transport"]
        );
        assert_eq!(
            empty.fallback.effective_on(),
            vec![FailureKind::Quota, FailureKind::RateLimit]
        );

        let default = parse_models_policy(&json!({}));
        assert!(default.fallback.disabled_by_config().is_empty());

        let partial = parse_models_policy(&json!({"fallback": {"on": ["transport"]}}));
        assert_eq!(
            partial.fallback.disabled_by_config(),
            vec!["empty_response"]
        );

        let recovery_only = parse_models_policy(&json!({"fallback": {"on": ["quota"]}}));
        assert_eq!(
            recovery_only.fallback.disabled_by_config(),
            vec!["empty_response", "transport"]
        );
    }
}
