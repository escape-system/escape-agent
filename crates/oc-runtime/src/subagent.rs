//! Subagent, sibling and digest tools (`docs/05-subagents.md`,
//! `docs/19-agentic-integration.md` §4).
//!
//! The tools are built per session in [`crate::turn`] and merged into the
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

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use oc_config::ConfigService;
use oc_llm::Catalog;
use oc_store::{
    Delivery, MessageQuery, NewInbox, NewSession, NewSessionTree, Order, SessionNodeRow,
};
use oc_subagents::{
    Limits, MessageError, ModelDefaults, NodeState, Policy, Role, SiblingBudget, SpawnBudget,
    SpawnError, WriteError, resolve_model,
};
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::payload::EventPayload;
use oc_types::{MessageId, ModelRef, SessionId, SessionInfo};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

/// Poll interval while a foreground `subagent` call waits for its child.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Grace allowed for an interrupted child to settle before a timeout report is
/// delivered to the parent.
const TIMEOUT_SETTLE_GRACE: Duration = Duration::from_millis(500);

/// Poll ticks a queued prompt may stay unpromoted before the child is
/// considered dead (2 s at [`POLL_INTERVAL`]).
const CHILD_START_GRACE_TICKS: usize = 20;

/// Digest cache capacity per runtime.
const DIGEST_CACHE_CAPACITY: usize = 64;
/// Digest requests allowed per caller per window.
const DIGEST_RATE_LIMIT: usize = 6;
/// Digest rate window in milliseconds.
const DIGEST_RATE_WINDOW_MS: i64 = 600_000;
/// Maximum transcript messages read by `digest`.
const DIGEST_MESSAGES: usize = 40;
/// Characters reserved for the child's complete final answer.
const DIGEST_FINAL_ANSWER_BUDGET: usize = 6_000;
/// Characters reserved for the excerpted recent-context section.
const DIGEST_CONTEXT_BUDGET: usize = 10_000;
/// Per-message cap for user, peer, and synthetic messages in recent context.
const DIGEST_CONTEXT_MESSAGE_CAP: usize = 400;
/// Per-message cap for non-final assistant messages in recent context.
const DIGEST_CONTEXT_ASSISTANT_CAP: usize = 400;

/// Maximum parent hops when authorizing a digest target against the caller's
/// subtree.
const DIGEST_ANCESTOR_STEPS: usize = 128;

/// Characters per token used only to bound the digest answer.
const CHARS_PER_TOKEN: usize = 4;
/// Guards ancestor walks against corrupt trees.
const MAX_ANCESTOR_STEPS: usize = 1024;

/// `subagent.peer.message` (ephemeral).
///
/// Published by `talk` after the peer inbox row is durable, so a UI can show
/// the message before the target session starts running. Peer messages are
/// not user messages and are never replayed from the event log.
#[derive(Debug, Serialize)]
struct SubagentPeerMessage {
    #[serde(rename = "sessionID")]
    session_id: String,
    from: String,
    agent: String,
    kind: String,
    text: String,
    #[serde(rename = "requestReply")]
    request_reply: bool,
    #[serde(rename = "messageID")]
    message_id: String,
}

impl EventPayload for SubagentPeerMessage {
    const TYPE: &'static str = "subagent.peer.message";
    const DURABLE: bool = false;
    const VERSION: u32 = 1;
}

/// Resolved `subagents` configuration plus the per-agent overrides the tools
/// need.
#[derive(Debug, Clone)]
pub(crate) struct SubagentSettings {
    limits: Limits,
    timeout: Duration,
    background_default: bool,
    digest_model: Option<String>,
    siblings_wake: bool,
    overrides: BTreeMap<String, AgentOverride>,
}

/// One `subagents.agents.<id>` entry.
#[derive(Debug, Clone, Default)]
struct AgentOverride {
    model: Option<String>,
    role: Option<String>,
    allow_spawn: Option<bool>,
    max_children: Option<u32>,
}

/// Per-runtime mutable tool state: rate budgets and the digest cache.
#[derive(Debug, Default)]
pub(crate) struct SubagentState {
    spawn: std::collections::HashMap<String, SpawnBudget>,
    siblings: std::collections::HashMap<String, SiblingBudget>,
    digests: DigestCache,
    digest_rate: std::collections::HashMap<String, VecDeque<i64>>,
}

impl SubagentState {
    fn spawn_budget(
        &mut self,
        session: &str,
        limits: &Limits,
        current_children: u32,
    ) -> &mut SpawnBudget {
        self.spawn.entry(session.to_owned()).or_insert_with(|| {
            SpawnBudget::with_default_rate(limits.max_children.saturating_sub(current_children))
        })
    }

    fn sibling_budget(&mut self, session: &str, limits: &Limits) -> &mut SiblingBudget {
        self.siblings
            .entry(session.to_owned())
            .or_insert_with(|| SiblingBudget::from_limits(limits))
    }
}

#[derive(Debug, Default)]
struct DigestCache {
    entries: VecDeque<(String, String)>,
}

impl DigestCache {
    fn get(&mut self, key: &str) -> Option<String> {
        let index = self.entries.iter().position(|(stored, _)| stored == key)?;
        let entry = self.entries.remove(index)?;
        let value = entry.1.clone();
        self.entries.push_back(entry);
        Some(value)
    }

    fn put(&mut self, key: String, value: String) {
        if let Some(index) = self.entries.iter().position(|(stored, _)| stored == &key) {
            self.entries.remove(index);
        }
        while self.entries.len() >= DIGEST_CACHE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }
}

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

#[derive(Debug, Clone)]
struct SubagentTool {
    runtime: Runtime,
    /// `subagents.background_default` captured when the tool was built, so the
    /// schema states the default the execution path will actually use.
    background_default: bool,
}

impl SubagentTool {
    /// Schema text for `background`, naming the configured default and its
    /// consequence.
    fn background_description(&self) -> String {
        if self.background_default {
            "Defaults to true (subagents.background_default): returns immediately with \
             {\"status\":\"running\"} and the child's final text arrives later as a synthetic \
             parent message. Pass false to wait for the result in this call."
                .to_owned()
        } else {
            "Defaults to false (subagents.background_default): waits for the child's final text \
             in this call. Pass true to return immediately and receive the result later as a \
             synthetic parent message."
                .to_owned()
        }
    }

    /// Schema text naming the agents the model may spawn. Read per request so
    /// agents added to the configuration appear on the next turn.
    fn agent_description(&self) -> String {
        let config = &self.runtime.inner.config.config;
        let default_agent = config
            .merged()
            .pointer("/subagents/default_agent")
            .and_then(Value::as_str)
            .unwrap_or("general");
        let available = available_agents(config);
        if available.is_empty() {
            format!(
                "Subagent id to spawn. Omit it to use the configured default `{default_agent}`."
            )
        } else {
            format!(
                "Subagent id to spawn, one of: {}. Omit it to use the configured default \
                 `{default_agent}`.",
                available.join(", ")
            )
        }
    }
}

#[derive(Debug, Clone)]
struct TalkTool {
    runtime: Runtime,
}

#[derive(Debug, Clone)]
struct RosterTool {
    runtime: Runtime,
}

#[derive(Debug, Clone)]
struct WriteSessionTool {
    runtime: Runtime,
}

#[derive(Debug, Clone)]
struct DigestTool {
    runtime: Runtime,
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

fn node_from_row(row: &SessionNodeRow) -> oc_subagents::SessionNode {
    oc_subagents::SessionNode {
        id: row.id.clone(),
        parent_id: row.parent_id.clone(),
        root_id: row.root_id.clone(),
        depth: row.depth,
        role: Role::from_column(&row.role).unwrap_or(Role::Subagent),
        spawn_allowed: row.spawn_allowed,
        child_count: row.child_count,
        agent: row.agent.clone(),
        model: row.model.clone(),
        state: NodeState::from_status(&row.status).unwrap_or(NodeState::Idle),
    }
}

async fn load_node(runtime: &Runtime, id: &str) -> Result<oc_subagents::SessionNode, ToolError> {
    runtime
        .inner
        .config
        .store
        .session_node(id)
        .await
        .map_err(|error| store_error(&error))?
        .as_ref()
        .map(node_from_row)
        .ok_or_else(|| ToolError::new(format!("SessionNotFoundError: session {id} does not exist")))
}

async fn load_info(runtime: &Runtime, id: &str) -> Result<SessionInfo, ToolError> {
    let value = runtime
        .inner
        .config
        .store
        .get_session(id)
        .await
        .map_err(|error| store_error(&error))?
        .ok_or_else(|| {
            ToolError::new(format!("SessionNotFoundError: session {id} does not exist"))
        })?;
    Runtime::parse_session(&value).map_err(|error| ToolError::new(error.to_string()))
}

fn store_error(error: &oc_store::StoreError) -> ToolError {
    ToolError::new(format!("StoreError: {error}"))
}

/// Only the caller itself or one of its descendants may be summarized, so a
/// child can never read a sibling, an ancestor, or an unrelated transcript.
async fn authorize_digest(runtime: &Runtime, caller: &str, target: &str) -> Result<(), ToolError> {
    if target == caller {
        return Ok(());
    }
    let store = &runtime.inner.config.store;
    let mut cursor = store
        .session_node(target)
        .await
        .map_err(|error| store_error(&error))?
        .and_then(|row| row.parent_id);
    for _ in 0..DIGEST_ANCESTOR_STEPS {
        let Some(current) = cursor.take() else {
            break;
        };
        if current == caller {
            return Ok(());
        }
        cursor = store
            .session_node(&current)
            .await
            .map_err(|error| store_error(&error))?
            .and_then(|row| row.parent_id);
    }
    Err(ToolError::new(format!(
        "DigestScopeError: session {target} is outside your subtree"
    )))
}

/// Load `ids` plus every ancestor into a tree usable by the policy checks.
async fn load_tree(
    runtime: &Runtime,
    ids: &[String],
) -> Result<BTreeMap<String, oc_subagents::SessionNode>, ToolError> {
    let store = &runtime.inner.config.store;
    let mut tree = BTreeMap::new();
    for id in ids {
        let mut cursor = Some(id.clone());
        for _ in 0..MAX_ANCESTOR_STEPS {
            let Some(current) = cursor.take() else {
                break;
            };
            if tree.contains_key(&current) {
                break;
            }
            let Some(row) = store
                .session_node(&current)
                .await
                .map_err(|error| store_error(&error))?
            else {
                break;
            };
            cursor.clone_from(&row.parent_id);
            tree.insert(current, node_from_row(&row));
        }
    }
    Ok(tree)
}

fn settings(runtime: &Runtime) -> SubagentSettings {
    load_settings(&runtime.inner.config.config)
}

fn available_agents(config: &ConfigService) -> Vec<String> {
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

fn available_models(catalog: &Catalog) -> Vec<String> {
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

fn model_defaults(config: &ConfigService, settings: &SubagentSettings) -> ModelDefaults {
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

fn spawn_error(error: &SpawnError) -> ToolError {
    ToolError::new(format!("SpawnForbiddenError: {error}"))
}

fn message_error(error: &MessageError) -> ToolError {
    ToolError::new(format!("SiblingScopeError: {error}"))
}

fn write_error(error: &WriteError) -> ToolError {
    ToolError::new(format!("WriteScopeError: {error}"))
}

fn budget_error(error: &oc_subagents::BudgetError) -> ToolError {
    ToolError::new(format!("BudgetError: {error}"))
}

fn json_error(error: &oc_llm::LlmError) -> ToolError {
    ToolError::new(format!("ModelUnavailableError: {error}"))
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("InvalidToolInput: {key} is required")))
}

fn optional_str(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_bool(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

fn requested_role(input: &Value, settings: &SubagentSettings, agent: &str) -> Role {
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

fn model_value(reference: &str, catalog: &Catalog) -> Result<Value, ToolError> {
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

fn xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

fn envelope(state: &str, session: &str, description: Option<&str>, text: &str) -> String {
    match description {
        Some(description) => format!(
            "<subagent sessionID=\"{session}\" state=\"{state}\" description=\"{}\">{text}</subagent>",
            xml_attr(description)
        ),
        None => format!("<subagent sessionID=\"{session}\" state=\"{state}\">{text}</subagent>"),
    }
}

fn envelope_reason(
    state: &str,
    session: &str,
    description: Option<&str>,
    reason: &str,
    text: &str,
) -> String {
    match description {
        Some(description) => format!(
            "<subagent sessionID=\"{session}\" state=\"{state}\" description=\"{}\" reason=\"{}\">{text}</subagent>",
            xml_attr(description),
            xml_attr(reason)
        ),
        None => format!(
            "<subagent sessionID=\"{session}\" state=\"{state}\" reason=\"{}\">{text}</subagent>",
            xml_attr(reason)
        ),
    }
}

fn message_text(message: &Value) -> String {
    if let Some(text) = message.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part.get("text").and_then(Value::as_str),
                    _ => None,
                })
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn final_assistant_text(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let text = message_text(message);
        if !text.trim().is_empty() {
            return text;
        }
        if let Some(error) = message
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return error.to_owned();
        }
    }
    String::new()
}

fn child_state(messages: &[Value]) -> String {
    for message in messages.iter().rev() {
        if message.get("type").and_then(Value::as_str) != Some("idle") {
            continue;
        }
        return match message.get("outcome").and_then(Value::as_str) {
            Some("failed") => "failed".to_owned(),
            Some("interrupted") => "cancelled".to_owned(),
            _ => "completed".to_owned(),
        };
    }
    "completed".to_owned()
}

/// Messages appended after the prompt marker. The marker is absent until the
/// child actor promotes the inbox row, so a pre-execution idle can never be
/// mistaken for completion.
fn after_prompt<'a>(messages: &'a [Value], prompt_id: &str) -> (bool, &'a [Value]) {
    match messages
        .iter()
        .position(|message| message.get("id").and_then(Value::as_str) == Some(prompt_id))
    {
        Some(index) => (true, &messages[index + 1..]),
        None => (false, &[]),
    }
}

/// Poll the child until this prompt's turn has produced a final answer.
///
/// Completion requires the prompt marker to exist **and** either a non-empty
/// final assistant text while idle, or a durable failure/cancel outcome. An
/// idle session before the marker is delivered (or before any assistant
/// message exists) keeps polling until the start grace expires: it is the
/// actor's pre-execution state, not a completed turn.
///
/// Error paths that bypass [`crate::turn`]'s settle step leave no idle row, so
/// the child session's existence, an errored assistant message and a prompt
/// that was never promoted are all treated as terminal failures.
///
/// `detach_on` is the session whose `POST /background` request ends the wait:
/// when it is set, one pending request is consumed and the wait returns
/// `("detached", "")` **without** touching the child. Watchers pass `None`, so
/// a detach request can never shorten a background delivery.
async fn wait_child(
    runtime: &Runtime,
    child: &str,
    prompt_id: &str,
    timeout: Option<Duration>,
    detach_on: Option<&str>,
) -> Result<(String, String), ToolError> {
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    let store = &runtime.inner.config.store;
    let mut observed_activity = false;
    let mut unstarted_ticks = 0usize;
    loop {
        if runtime.inner.shutdown.is_cancelled() {
            return Err(runtime_error(&crate::RuntimeError::Shutdown));
        }
        if let Some(session) = detach_on
            && runtime.take_background_request(session)
        {
            return Ok(("detached".to_owned(), String::new()));
        }
        let exists = store
            .get_session(child)
            .await
            .map_err(|error| store_error(&error))?
            .is_some();
        if !exists {
            return Ok(("failed".to_owned(), "session deleted".to_owned()));
        }
        let idle = runtime
            .is_idle(child)
            .await
            .map_err(|error| runtime_error(&error))?;
        let messages = runtime
            .list_all_messages(child)
            .await
            .map_err(|error| runtime_error(&error))?;
        let (started, tail) = after_prompt(&messages, prompt_id);
        let has_assistant = tail
            .iter()
            .any(|message| message.get("type").and_then(Value::as_str) == Some("assistant"));
        if started && !idle {
            observed_activity = true;
        }
        if !started && messages.is_empty() && idle {
            unstarted_ticks += 1;
            if unstarted_ticks >= CHILD_START_GRACE_TICKS {
                return Ok(("failed".to_owned(), "child never started".to_owned()));
            }
        } else {
            unstarted_ticks = 0;
        }
        if idle {
            let state = child_state(tail);
            if matches!(state.as_str(), "failed" | "cancelled") {
                let text = match final_assistant_text(tail).trim() {
                    "" => format!("subagent {state}"),
                    text => text.to_owned(),
                };
                return Ok((state, text));
            }
            if let Some((state, text)) = assistant_failure(tail) {
                return Ok((state, text));
            }
            if has_assistant {
                let text = final_assistant_text(tail);
                if text.trim().is_empty() {
                    return Ok((
                        "failed".to_owned(),
                        "subagent finished without final text".to_owned(),
                    ));
                }
                return Ok(("completed".to_owned(), text));
            }
            if started && observed_activity {
                return Ok((
                    "failed".to_owned(),
                    "subagent ended without a final message".to_owned(),
                ));
            }
        }
        if let Some(deadline) = deadline
            && tokio::time::Instant::now() >= deadline
        {
            return Ok(("running".to_owned(), String::new()));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The final assistant message failed without a settle idle row.
fn assistant_failure(tail: &[Value]) -> Option<(String, String)> {
    let message = tail
        .iter()
        .rev()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))?;
    let error = message.get("error");
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let finish = message
        .get("finish")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if finish != "error" && error.is_none() {
        return None;
    }
    let state = if error_type.starts_with("MessageAborted") {
        "cancelled"
    } else {
        "failed"
    };
    let text = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| final_assistant_text(std::slice::from_ref(message)));
    Some((state.to_owned(), text))
}

fn runtime_error(error: &crate::RuntimeError) -> ToolError {
    ToolError::new(format!("RuntimeError: {error}"))
}

fn spawn_watcher(
    runtime: Runtime,
    parent: String,
    child: String,
    agent: String,
    description: String,
    prompt_id: String,
    timeout: Duration,
) {
    tokio::spawn(async move {
        let outcome = match wait_child(&runtime, &child, &prompt_id, Some(timeout), None).await {
            Ok((state, _)) if state == "running" => {
                interrupt_timed_out(&runtime, &child, &prompt_id).await;
                timeout_failure(timeout)
            }
            Ok((state, text)) => (state, text, None),
            Err(error) => {
                let reason = error.message.clone();
                ("failed".to_owned(), reason.clone(), Some(reason))
            }
        };
        deliver_subagent_result(&runtime, &parent, &child, &agent, &description, outcome).await;
    });
}

/// Reports a timeout whose budget was already consumed by the caller.
///
/// The foreground path interrupts the child itself, so the watcher only gives
/// the actor a bounded settle window and then delivers the single timeout
/// failure. It never waits another full timeout.
fn spawn_timeout_watcher(
    runtime: Runtime,
    parent: String,
    child: String,
    agent: String,
    description: String,
    prompt_id: String,
    timeout: Duration,
) {
    tokio::spawn(async move {
        let _ = wait_child(
            &runtime,
            &child,
            &prompt_id,
            Some(TIMEOUT_SETTLE_GRACE),
            None,
        )
        .await;
        deliver_subagent_result(
            &runtime,
            &parent,
            &child,
            &agent,
            &description,
            timeout_failure(timeout),
        )
        .await;
    });
}

/// Interrupts `child` when the parent's execution token is cancelled. It runs
/// detached from the tool future, which the turn loop drops on cancellation.
fn spawn_cancel_guard(
    runtime: &Runtime,
    child: &str,
    prompt_id: &str,
    token: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let runtime = runtime.clone();
    let child = child.to_owned();
    let prompt_id = prompt_id.to_owned();
    tokio::spawn(async move {
        token.cancelled().await;
        interrupt_timed_out(&runtime, &child, &prompt_id).await;
    })
}

/// Cancels a child that exhausted its timeout so its actor settles and any
/// pending permission ask is dropped, then waits a bounded settle grace.
async fn interrupt_timed_out(runtime: &Runtime, child: &str, prompt_id: &str) {
    match runtime.interrupt(child).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(child = %child, "timed-out subagent had no running execution to interrupt");
        }
        Err(error) => {
            tracing::warn!(child = %child, error = %error, "timed-out subagent could not be interrupted");
        }
    }
    let _ = wait_child(runtime, child, prompt_id, Some(TIMEOUT_SETTLE_GRACE), None).await;
}

fn timeout_failure(timeout: Duration) -> (String, String, Option<String>) {
    let reason = format!("timeout after {}s", timeout.as_secs());
    ("failed".to_owned(), reason.clone(), Some(reason))
}

pub(crate) async fn deliver_subagent_result(
    runtime: &Runtime,
    parent: &str,
    child: &str,
    agent: &str,
    description: &str,
    (state, text, reason): (String, String, Option<String>),
) {
    let text = if text.trim().is_empty() {
        "subagent failed".to_owned()
    } else {
        text
    };
    let body = match &reason {
        Some(reason) => envelope_reason(&state, child, Some(description), reason, &text),
        None => envelope(&state, child, Some(description), &text),
    };
    let mut metadata = json!({
        "source": "subagent",
        "childID": child,
        "agent": agent,
        "state": state,
    });
    if let Some(reason) = &reason {
        metadata["reason"] = json!(reason);
    }
    let payload = json!({ "text": body, "metadata": metadata });
    let item = NewInbox {
        id: MessageId::new().into_inner(),
        session_id: parent.to_owned(),
        kind: "synthetic".to_owned(),
        payload,
        delivery: Delivery::Steer,
    };
    if let Err(error) = runtime.inner.config.store.enqueue(item).await {
        tracing::warn!(parent = %parent, error = %error, "subagent completion was not delivered");
        return;
    }
    if let Err(error) = runtime.notify(parent).await {
        tracing::warn!(parent = %parent, error = %error, "subagent completion did not wake the parent");
    }
}

async fn enqueue_prompt(
    runtime: &Runtime,
    session: &str,
    prompt: &str,
    parent: &str,
) -> Result<String, ToolError> {
    let id = MessageId::new().into_inner();
    runtime
        .inner
        .config
        .store
        .enqueue(NewInbox {
            id: id.clone(),
            session_id: session.to_owned(),
            kind: "user".to_owned(),
            payload: json!({
                "text": prompt,
                "metadata": { "source": "subagent", "parentID": parent },
            }),
            delivery: Delivery::Steer,
        })
        .await
        .map_err(|error| store_error(&error))?;
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
async fn finish_foreground(
    runtime: &Runtime,
    parent: &str,
    child: &str,
    agent: &str,
    description: &str,
    prompt_id: &str,
    settings: &SubagentSettings,
    cancel: Option<&CancellationToken>,
) -> Result<ToolResult, ToolError> {
    // The turn loop drops the tool future as soon as the execution is
    // cancelled; this detached guard is what stops the child in that case, so
    // an interrupted parent cannot leave an orphan burning tokens.
    let guard = cancel.map(|token| spawn_cancel_guard(runtime, child, prompt_id, token.clone()));
    let outcome = wait_child(
        runtime,
        child,
        prompt_id,
        Some(settings.timeout),
        Some(parent),
    )
    .await;
    if let Some(guard) = guard {
        guard.abort();
    }
    let (state, text) = outcome?;
    let running = json!({ "sessionID": child, "status": "running" });
    if state == "detached" {
        // `POST /background` arrived while this turn waited on the child: hand
        // the wait to a watcher so the turn can continue. The child is not
        // interrupted; its final text is delivered as a synthetic message.
        spawn_watcher(
            runtime.clone(),
            parent.to_owned(),
            child.to_owned(),
            agent.to_owned(),
            description.to_owned(),
            prompt_id.to_owned(),
            settings.timeout,
        );
        return Ok(ToolResult::text(running.to_string()).with_metadata(running));
    }
    if state == "running" {
        // The child consumed its full budget: cancel it now so it settles and
        // any pending permission ask is dropped, then report the timeout with
        // a short settle window instead of waiting another full timeout.
        match runtime.interrupt(child).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(child = %child, "timed-out subagent had no running execution to interrupt");
            }
            Err(error) => {
                tracing::warn!(child = %child, error = %error, "timed-out subagent could not be interrupted");
            }
        }
        spawn_timeout_watcher(
            runtime.clone(),
            parent.to_owned(),
            child.to_owned(),
            agent.to_owned(),
            description.to_owned(),
            prompt_id.to_owned(),
            settings.timeout,
        );
        return Ok(ToolResult::text(
            json!({ "sessionID": child, "status": "running" }).to_string(),
        )
        .with_metadata(json!({ "sessionID": child, "status": "running" })));
    }
    let body = envelope(&state, child, None, &text);
    Ok(ToolResult::text(body).with_metadata(json!({ "sessionID": child, "status": state })))
}

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "subagent"
    }

    fn description(&self) -> &'static str {
        "Create a child session for a subagent, or continue an existing child. Call this before your own exploration when the user asks you to spawn subagents. Depending on `background` (default from configuration) it waits for the final text or returns immediately and delivers the result as a synthetic message. Use it to delegate self-contained work that can run in parallel; `agent` may be omitted to use the configured default. The parent sees only the final text; use digest for transcripts."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": { "type": "string", "description": self.agent_description() },
                "description": { "type": "string", "description": "Short label shown in the session tree." },
                "prompt": { "type": "string", "description": "Task text delivered to the child." },
                "model": { "type": "string", "description": "Model override: provider/id or provider/id#variant." },
                "sessionID": { "type": "string", "description": "Continue an existing direct child or descendant." },
                "background": { "type": "boolean", "description": self.background_description() },
                "role": { "type": "string", "enum": ["worker", "coordinator"], "description": "Child policy role." },
                "spawn": {
                    "type": "object",
                    "properties": {
                        "allow": { "type": "boolean", "description": "Allow the child to spawn its own children." },
                        "max_depth": { "type": "integer", "minimum": 0 }
                    },
                    "additionalProperties": false
                },
                "lane": { "type": "string", "description": "Lane hint recorded for the lane service." }
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let settings = settings(runtime);
        let agent =
            optional_str(&input, "agent").unwrap_or_else(|| settings.limits.default_agent.clone());
        let description = required_str(&input, "description")?;
        let prompt = required_str(&input, "prompt")?;
        let background = optional_bool(&input, "background").unwrap_or(settings.background_default);

        if let Some(target) = optional_str(&input, "sessionID") {
            return continue_child(
                runtime,
                &caller,
                &target,
                &prompt,
                &description,
                background,
                &settings,
                ctx.cancel.as_ref(),
            )
            .await;
        }

        let parent = load_node(runtime, &caller).await?;
        let mut limits = settings.limits.clone();
        if let Some(over) = settings.overrides.get(&parent.agent) {
            if over.allow_spawn == Some(false) {
                return Err(spawn_error(&SpawnError::SpawnNotAllowed { id: caller }));
            }
            if let Some(max_children) = over.max_children {
                limits.max_children = max_children;
            }
        }
        let available = available_agents(&runtime.inner.config.config);
        let policy = Policy::new(limits.clone());
        let requested_depth = input
            .get("spawn")
            .and_then(|spawn| spawn.get("max_depth"))
            .and_then(Value::as_u64)
            .map(|depth| u32::try_from(depth).unwrap_or(u32::MAX));
        let mut plan = policy
            .spawn(&parent, &agent, requested_depth, &available)
            .map_err(|error| spawn_error(&error))?;

        if let Some(over) = settings.overrides.get(&agent)
            && over.allow_spawn == Some(false)
        {
            plan.spawn_allowed = false;
        }
        if input
            .get("spawn")
            .and_then(|spawn| spawn.get("allow"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            plan.spawn_allowed = false;
        }

        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .spawn_budget(&caller, &limits, parent.child_count)
                .try_spawn(oc_types::now_millis())
                .map_err(|error| budget_error(&error))?;
        }

        let parent_model = parent
            .model
            .clone()
            .or_else(|| runtime.inner.config.config.model())
            .unwrap_or_default();
        let defaults = model_defaults(&runtime.inner.config.config, &settings);
        let choice = resolve_model(
            optional_str(&input, "model").as_deref(),
            Some(&agent),
            defaults,
            &parent_model,
            &available_models(&runtime.inner.config.catalog),
        )
        .map_err(|error| ToolError::new(format!("ModelUnavailableError: {error}")))?;
        let model_value = model_value(&choice.reference, &runtime.inner.config.catalog)?;
        let parent_info = load_info(runtime, &caller).await?;
        let child = SessionId::new().into_inner();
        let role = requested_role(&input, &settings, &agent);
        let lane = optional_str(&input, "lane");
        let metadata = lane.map(|lane| json!({ "lane": lane }));
        runtime
            .inner
            .config
            .store
            .create_session_with_tree(
                NewSession {
                    id: child.clone(),
                    parent_id: Some(caller.clone()),
                    project_id: parent_info.project_id.to_string(),
                    directory: parent_info.location.directory.clone(),
                    title: Some(description.clone()),
                    agent: Some(agent.clone()),
                    model: Some(model_value),
                    metadata,
                    permissions: None,
                    time_created: oc_types::now_millis(),
                },
                NewSessionTree {
                    root_id: parent.root_id.clone(),
                    depth: plan.depth,
                    role: role.as_str().to_owned(),
                    spawn_allowed: plan.spawn_allowed,
                },
            )
            .await
            .map_err(|error| store_error(&error))?;

        let prompt_id = enqueue_prompt(runtime, &child, &prompt, &caller).await?;
        runtime
            .notify(&child)
            .await
            .map_err(|error| runtime_error(&error))?;

        if background {
            spawn_watcher(
                runtime.clone(),
                caller.clone(),
                child.clone(),
                agent,
                description,
                prompt_id,
                settings.timeout,
            );
            return Ok(ToolResult::text(
                json!({ "sessionID": child, "status": "running" }).to_string(),
            )
            .with_metadata(json!({ "sessionID": child, "status": "running" })));
        }
        finish_foreground(
            runtime,
            &caller,
            &child,
            &agent,
            &description,
            &prompt_id,
            &settings,
            ctx.cancel.as_ref(),
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn continue_child(
    runtime: &Runtime,
    caller: &str,
    target: &str,
    prompt: &str,
    description: &str,
    background: bool,
    settings: &SubagentSettings,
    cancel: Option<&CancellationToken>,
) -> Result<ToolResult, ToolError> {
    let caller_node = load_node(runtime, caller).await?;
    let target_node = load_node(runtime, target).await?;
    let tree = load_tree(runtime, &[caller.to_owned(), target.to_owned()]).await?;
    Policy::new(settings.limits.clone())
        .can_write(&caller_node, &target_node, &tree)
        .map_err(|error| write_error(&error))?;
    let prompt_id = enqueue_prompt(runtime, target, prompt, caller).await?;
    runtime
        .notify(target)
        .await
        .map_err(|error| runtime_error(&error))?;
    if background {
        spawn_watcher(
            runtime.clone(),
            caller.to_owned(),
            target.to_owned(),
            target_node.agent.clone(),
            description.to_owned(),
            prompt_id,
            settings.timeout,
        );
        return Ok(ToolResult::text(
            json!({ "sessionID": target, "status": "running" }).to_string(),
        )
        .with_metadata(json!({ "sessionID": target, "status": "running" })));
    }
    finish_foreground(
        runtime,
        caller,
        target,
        &target_node.agent,
        description,
        &prompt_id,
        settings,
        cancel,
    )
    .await
}

#[async_trait::async_trait]
impl Tool for TalkTool {
    fn name(&self) -> &'static str {
        "talk"
    }

    fn description(&self) -> &'static str {
        "Send a peer note, question or answer to a sibling, parent or child session. The target receives a <peer> block."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Target session id." },
                "kind": { "type": "string", "enum": ["note", "question", "answer"] },
                "text": { "type": "string" },
                "request_reply": { "type": "boolean", "description": "Ask the target to answer when it replies." }
            },
            "required": ["to", "kind", "text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let to = required_str(&input, "to")?;
        let kind = required_str(&input, "kind")?;
        let text = required_str(&input, "text")?;
        if !matches!(kind.as_str(), "note" | "question" | "answer") {
            return Err(ToolError::new(
                "InvalidToolInput: kind must be note, question or answer",
            ));
        }
        let request_reply = optional_bool(&input, "request_reply").unwrap_or(false);
        let settings = settings(runtime);
        let from_node = load_node(runtime, &caller).await?;
        let to_node = load_node(runtime, &to).await?;
        let to_info = load_info(runtime, &to).await?;
        let tree = load_tree(runtime, &[caller.clone(), to.clone()]).await?;
        Policy::new(settings.limits.clone())
            .can_message(&from_node, &to_node, &tree)
            .map_err(|error| message_error(&error))?;

        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .sibling_budget(&caller, &settings.limits)
                .try_message(oc_types::now_millis())
                .map_err(|error| budget_error(&error))?;
        }

        let message_id = MessageId::new().into_inner();
        runtime
            .inner
            .config
            .store
            .enqueue(NewInbox {
                id: message_id.clone(),
                session_id: to.clone(),
                kind: "peer".to_owned(),
                payload: json!({
                    "text": text,
                    "kind": kind,
                    "from": caller,
                    "agent": from_node.agent,
                    "sender_session_id": caller,
                    "sender_agent": from_node.agent,
                    "request_reply": request_reply,
                    "metadata": {
                        "source": "peer",
                        "from": caller,
                        "agent": from_node.agent,
                        "kind": kind,
                    },
                }),
                delivery: Delivery::Steer,
            })
            .await
            .map_err(|error| store_error(&error))?;
        // The inbox row is durable; publish the ephemeral UI event before the
        // target is woken so a client sees the message before output starts.
        runtime
            .emit(
                &to,
                &to_info.location.directory,
                SubagentPeerMessage {
                    session_id: to.clone(),
                    from: caller.clone(),
                    agent: from_node.agent.clone(),
                    kind: kind.clone(),
                    text: text.clone(),
                    request_reply,
                    message_id,
                },
            )
            .await
            .map_err(|error| runtime_error(&error))?;
        if settings.siblings_wake {
            runtime
                .notify(&to)
                .await
                .map_err(|error| runtime_error(&error))?;
        }
        Ok(ToolResult::text(format!("{kind} delivered to {to}")))
    }
}

#[async_trait::async_trait]
impl Tool for RosterTool {
    fn name(&self) -> &'static str {
        "roster"
    }

    fn description(&self) -> &'static str {
        "List the parent, siblings and children of this session. Returns ids, agents, statuses and titles only."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let store = &runtime.inner.config.store;
        let node = load_node(runtime, &caller).await?;
        let mut children = store
            .children(&caller)
            .await
            .map_err(|error| store_error(&error))?;
        let parent = match &node.parent_id {
            Some(parent) => store
                .session_node(parent)
                .await
                .map_err(|error| store_error(&error))?,
            None => None,
        };
        let siblings = match &node.parent_id {
            Some(parent) => store
                .children(parent)
                .await
                .map_err(|error| store_error(&error))?
                .into_iter()
                .filter(|child| child.id != caller)
                .collect(),
            None => Vec::new(),
        };
        children.sort_by(|left, right| left.id.cmp(&right.id));
        let mut siblings: Vec<SessionNodeRow> = siblings;
        siblings.sort_by(|left, right| left.id.cmp(&right.id));

        let mut parent_value = Value::Null;
        if let Some(parent) = &parent {
            parent_value = roster_entry(runtime, parent).await?;
        }
        let mut sibling_values = Vec::new();
        for sibling in &siblings {
            sibling_values.push(roster_entry(runtime, sibling).await?);
        }
        let mut child_values = Vec::new();
        for child in &children {
            child_values.push(roster_entry(runtime, child).await?);
        }
        let body = json!({
            "parent": parent_value,
            "siblings": sibling_values,
            "children": child_values,
        });
        Ok(ToolResult::text(body.to_string()).with_metadata(body))
    }
}

async fn roster_entry(runtime: &Runtime, node: &SessionNodeRow) -> Result<Value, ToolError> {
    let mut value = Map::new();
    value.insert("id".to_owned(), Value::String(node.id.clone()));
    value.insert("agent".to_owned(), Value::String(node.agent.clone()));
    value.insert("status".to_owned(), Value::String(node.status.clone()));
    let session = runtime
        .inner
        .config
        .store
        .get_session(&node.id)
        .await
        .map_err(|error| store_error(&error))?;
    if let Some(title) = session
        .as_ref()
        .and_then(|session| session.get("title"))
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
    {
        value.insert("title".to_owned(), Value::String(title.to_owned()));
    }
    Ok(Value::Object(value))
}

#[async_trait::async_trait]
impl Tool for WriteSessionTool {
    fn name(&self) -> &'static str {
        "session_write"
    }

    fn description(&self) -> &'static str {
        "Write text into a descendant session (or self). Siblings and parents are rejected by policy."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Target session id." },
                "text": { "type": "string" },
                "delivery": { "type": "string", "enum": ["steer", "queue"] },
                "interrupt": { "type": "boolean", "description": "Interrupt the target's current execution first." }
            },
            "required": ["session_id", "text"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let to = required_str(&input, "session_id")?;
        let text = required_str(&input, "text")?;
        let settings = settings(runtime);
        let delivery = match optional_str(&input, "delivery").as_deref() {
            None | Some("steer") => Delivery::Steer,
            Some("queue") => Delivery::Queue,
            Some(other) => {
                return Err(ToolError::new(format!(
                    "InvalidToolInput: unknown delivery {other:?}"
                )));
            }
        };
        let from_node = load_node(runtime, &caller).await?;
        let to_node = load_node(runtime, &to).await?;
        let tree = load_tree(runtime, &[caller.clone(), to.clone()]).await?;
        Policy::new(settings.limits.clone())
            .can_write(&from_node, &to_node, &tree)
            .map_err(|error| write_error(&error))?;

        if optional_bool(&input, "interrupt").unwrap_or(false) {
            runtime
                .interrupt(&to)
                .await
                .map_err(|error| runtime_error(&error))?;
        }
        runtime
            .inner
            .config
            .store
            .enqueue(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: to.clone(),
                kind: "synthetic".to_owned(),
                payload: json!({
                    "text": text,
                    "metadata": { "source": "peer", "from": caller },
                }),
                delivery,
            })
            .await
            .map_err(|error| store_error(&error))?;
        runtime
            .notify(&to)
            .await
            .map_err(|error| runtime_error(&error))?;
        Ok(ToolResult::text(format!("wrote to {to}")))
    }
}

#[async_trait::async_trait]
impl Tool for DigestTool {
    fn name(&self) -> &'static str {
        "digest"
    }

    fn description(&self) -> &'static str {
        "Summarize a session transcript with the scribe model. Never returns raw tool output."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Session to summarize; defaults to this session." },
                "tool_call_id": { "type": "string", "description": "Focus on one tool call." },
                "question": { "type": "string", "description": "What to answer about the transcript." },
                "max_tokens": { "type": "integer", "minimum": 1, "description": "Answer budget, default 300." }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let runtime = &self.runtime;
        let caller = ctx.session_id.clone();
        let target = optional_str(&input, "session_id").unwrap_or_else(|| caller.clone());
        let question = optional_str(&input, "question")
            .unwrap_or_else(|| "Summarize what this session did and any errors.".to_owned());
        let tool_call_id = optional_str(&input, "tool_call_id");
        let max_tokens = input
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(1, 4096);
        let settings = settings(runtime);
        let scribe = runtime.scribe_model().ok_or_else(|| {
            let configured = settings
                .digest_model
                .as_deref()
                .map_or(String::new(), |model| format!(" (configured {model})"));
            ToolError::new(format!(
                "DigestUnavailableError: digest requires a scribe model{configured}"
            ))
        })?;
        load_info(runtime, &target).await?;
        authorize_digest(runtime, &caller, &target).await?;
        let page = runtime
            .inner
            .config
            .store
            .list_messages(MessageQuery {
                session_id: target.clone(),
                limit: DIGEST_MESSAGES,
                order: Order::Desc,
                cursor: None,
                kind: None,
            })
            .await
            .map_err(|error| store_error(&error))?;
        let excerpts = digest_excerpts(&page.items);
        let hash = digest_hash(&target, tool_call_id.as_deref(), &question, &excerpts);
        let key = format!("{target}:{hash}");
        if let Some(cached) = runtime
            .inner
            .subagents
            .lock()
            .expect("subagent state")
            .digests
            .get(&key)
        {
            // The cache stores the raw model answer, so each caller still gets
            // its own token budget instead of the first caller's truncation.
            let text = truncate_tokens(&cached, max_tokens);
            return Ok(ToolResult::text(text).with_metadata(json!({ "cached": true })));
        }
        {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            check_digest_rate(&mut state, &caller)?;
        }
        let prompt = format!(
            "Answer the question about this session transcript. Be concise and factual. \
             Never quote raw tool output verbatim.\n\
             The transcript is excerpted by design: the FINAL ANSWER section holds the \
             session's complete latest answer, while RECENT CONTEXT is a shortened excerpt \
             of earlier messages. Answer from what is present and do not refuse, apologize, \
             or mention truncation just because the context is excerpted.\n\
             Question: {question}\n\
             Tool call: {}\n\nTranscript:\n{excerpts}",
            tool_call_id.as_deref().unwrap_or("(none)")
        );
        let response = scribe
            .complete(&prompt)
            .await
            .map_err(|error| ToolError::new(format!("DigestError: {error}")))?;
        let text = truncate_tokens(&response, max_tokens);
        runtime
            .inner
            .subagents
            .lock()
            .expect("subagent state")
            .digests
            .put(key, response);
        Ok(ToolResult::text(text).with_metadata(json!({ "cached": false })))
    }
}

fn check_digest_rate(state: &mut SubagentState, caller: &str) -> Result<(), ToolError> {
    let now = oc_types::now_millis();
    let window = state.digest_rate.entry(caller.to_owned()).or_default();
    while window
        .front()
        .is_some_and(|stamp| now.saturating_sub(*stamp) >= DIGEST_RATE_WINDOW_MS)
    {
        window.pop_front();
    }
    if window.len() >= DIGEST_RATE_LIMIT {
        return Err(ToolError::new(
            "DigestRateLimitError: digest is limited to 6 requests per 10 minutes",
        ));
    }
    window.push_back(now);
    Ok(())
}

fn digest_hash(
    session: &str,
    tool_call_id: Option<&str>,
    question: &str,
    excerpts: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    hasher.update(b"\0");
    hasher.update(tool_call_id.unwrap_or_default().as_bytes());
    hasher.update(b"\0");
    hasher.update(question.as_bytes());
    hasher.update(b"\0");
    hasher.update(excerpts.as_bytes());
    hex::encode(hasher.finalize())
}

fn truncate_tokens(text: &str, max_tokens: u64) -> String {
    let limit = usize::try_from(max_tokens)
        .unwrap_or(usize::MAX)
        .saturating_mul(CHARS_PER_TOKEN);
    text.chars().take(limit).collect()
}

fn digest_excerpts(messages: &[Value]) -> String {
    // `messages` arrive newest-first (`Order::Desc`), so the first assistant
    // message with text is the session's final answer.
    let final_index = messages.iter().position(|message| {
        message.get("type").and_then(Value::as_str) == Some("assistant")
            && !message_text(message).trim().is_empty()
    });

    let mut out = String::new();
    if let Some(index) = final_index {
        let final_text = message_text(&messages[index]);
        out.push_str("FINAL ANSWER (complete):\n");
        push_bounded(
            &mut out,
            final_text.trim(),
            DIGEST_FINAL_ANSWER_BUDGET,
            &format!("…[final answer truncated at {DIGEST_FINAL_ANSWER_BUDGET} chars]"),
        );
        out.push('\n');
    }

    let mut context = String::new();
    let mut omitted = false;
    for (index, message) in messages.iter().enumerate() {
        if Some(index) == final_index {
            continue;
        }
        let mut entry = String::new();
        let kind = message.get("type").and_then(Value::as_str).unwrap_or("?");
        match kind {
            "user" | "synthetic" | "peer" => {
                entry.push_str(kind);
                entry.push_str(": ");
                push_bounded(
                    &mut entry,
                    message_text(message).trim(),
                    DIGEST_CONTEXT_MESSAGE_CAP,
                    "…[truncated]",
                );
                entry.push('\n');
            }
            "assistant" => {
                let text = message_text(message);
                if !text.trim().is_empty() {
                    entry.push_str("assistant: ");
                    push_bounded(
                        &mut entry,
                        text.trim(),
                        DIGEST_CONTEXT_ASSISTANT_CAP,
                        "…[truncated]",
                    );
                    entry.push('\n');
                }
                if let Some(parts) = message.get("content").and_then(Value::as_array) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) != Some("tool") {
                            continue;
                        }
                        let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let state = part
                            .get("state")
                            .and_then(|state| state.get("status"))
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let _ = writeln!(entry, "tool {name}: {state}");
                    }
                }
            }
            "idle" => {
                let outcome = message
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("idle");
                let _ = writeln!(entry, "idle: {outcome}");
            }
            _ => {}
        }
        if context.chars().count() + entry.chars().count() > DIGEST_CONTEXT_BUDGET {
            omitted = true;
            break;
        }
        context.push_str(&entry);
    }

    if !context.is_empty() || omitted {
        out.push_str("RECENT CONTEXT (excerpted):\n");
        out.push_str(&context);
        if omitted {
            let _ = writeln!(
                out,
                "…[older messages omitted beyond {DIGEST_CONTEXT_BUDGET} chars]"
            );
        }
    }
    out
}

/// Appends `text`, cutting it at `limit` characters when it exceeds the budget
/// and marking the cut explicitly.
fn push_bounded(out: &mut String, text: &str, limit: usize, marker: &str) {
    if text.chars().count() <= limit {
        out.push_str(text);
        return;
    }
    out.extend(text.chars().take(limit));
    out.push_str(marker);
}
