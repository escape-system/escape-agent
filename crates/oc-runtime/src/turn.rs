//! The turn loop: prompt promotion, streaming, tools, compaction and settlement.
//!
//! Durable message rows are written before the events that describe them. The
//! assistant message is appended once per step and rewritten at text end, tool
//! call, tool result and step end; deltas are never persisted.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use oc_config::Effect as ConfigEffect;
use oc_config::PermissionRule;
use oc_llm::{LlmMessage, LlmOptions, LlmRequest, ResolvedModel, ToolDef, Usage};
use oc_memory::{MemoryTools, NewMemoryInput};
use oc_store::NewMessage;
use oc_tools::{ToolContext, ToolRegistry};
use oc_types::message::{
    AssistantPart, FinishReason, ReasoningPart, StructuredError, TextPart, ToolContent, ToolPart,
    ToolState, ToolTime,
};
use oc_types::payload::{
    EventPayload, SessionExecutionFailed, SessionExecutionInterrupted, SessionExecutionSucceeded,
    SessionIdle, SessionInboxDelivered, SessionStatusUpdated, SessionStepEnded, SessionStepFailed,
    SessionStepStarted, SessionStepStreamed, SessionTextDelta, SessionTextEnded,
    SessionTextStarted, SessionToolCalled, SessionToolFailed, SessionToolInputDelta,
    SessionToolInputEnded, SessionToolInputStarted, SessionToolSuccess, SessionUsageUpdated,
};
use oc_types::{
    Effect as WireEffect, InterruptReason, MessageId, ModelRef, SessionInfo, SessionStatus, Tokens,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::expectation::{self, Expectation};
use crate::instructions::InstructionSet;
use crate::prompt::{self, MAX_STEPS_PROMPT};
use crate::runtime::{
    ActorHandle, CancelToken, PermissionAsk, PermissionDecision, Runtime, RuntimeError,
};
use crate::tokens;

const MAX_TOOL_PARALLELISM: usize = 8;
const DEFAULT_STEPS: u64 = 50;

/// Resolved session state for one execution.
#[derive(Clone, Debug)]
struct ResolvedTurn {
    info: SessionInfo,
    directory: String,
    agent_id: String,
    model: ResolvedModel,
    model_ref: ModelRef,
    agent_system: Option<String>,
    tool_names: Vec<String>,
    registry: Arc<ToolRegistry>,
    system: Vec<String>,
    tools: Vec<ToolDef>,
    memory_ids: Vec<String>,
    max_steps: usize,
    session_rules: Vec<PermissionRule>,
    context_window: u64,
}

/// Everything the turn observed that the scribe digest needs.
#[derive(Debug, Default)]
struct TurnStats {
    tool_calls: usize,
    user_text: String,
    assistant_text: String,
    tool_lines: Vec<String>,
}

/// `session.instructions.updated` (durable, v2).
///
/// `oc-types` owns the wire payload catalog; the runtime defines this locally
/// until the payload lands there so the instruction prefix can still publish
/// deltas. [`crate::Runtime::emit`] persists the payload before publishing it.
#[derive(Debug, Serialize)]
struct SessionInstructionsUpdated {
    #[serde(rename = "sessionID")]
    session_id: String,
    delta: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

impl EventPayload for SessionInstructionsUpdated {
    const TYPE: &'static str = "session.instructions.updated";
    const DURABLE: bool = true;
    const VERSION: u32 = 2;
}

#[derive(Debug)]
enum ExecutionOutcome {
    Succeeded,
    Failed(StructuredError),
    Interrupted(InterruptReason),
}

#[derive(Debug)]
struct StepRun {
    state: AssistantState,
    calls: Vec<PendingToolCall>,
    tokens: Tokens,
    cost: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingToolCall {
    index: usize,
    pub(crate) id: String,
    pub(crate) name: String,
    input: Map<String, Value>,
    pub(crate) expectation: Option<Expectation>,
    invalid: Option<StructuredError>,
}

enum AuthFailure {
    Cancelled,
    Denied(StructuredError),
}

#[derive(Debug)]
pub(crate) enum ToolOutcome {
    Completed {
        index: usize,
        input: Map<String, Value>,
        content: Vec<ToolContent>,
        metadata: Option<Map<String, Value>>,
        executed: bool,
    },
    Failed {
        index: usize,
        input: Map<String, Value>,
        error: StructuredError,
        content: Option<Vec<ToolContent>>,
        metadata: Option<Map<String, Value>>,
        executed: bool,
    },
    Cancelled,
}

/// Mutable assistant message for one step.
#[derive(Debug, Clone)]
struct AssistantState {
    id: MessageId,
    agent: String,
    model: ModelRef,
    created: i64,
    content: Vec<AssistantPart>,
    streamed: Option<i64>,
    completed: Option<i64>,
    finish: Option<FinishReason>,
    raw_finish: Option<String>,
    tokens: Option<Tokens>,
    cost: Option<f64>,
    error: Option<StructuredError>,
    open_text: Option<usize>,
    open_reasoning: Option<usize>,
}

impl AssistantState {
    fn new(id: MessageId, agent: String, model: ModelRef, created: i64) -> Self {
        Self {
            id,
            agent,
            model,
            created,
            content: Vec::new(),
            streamed: None,
            completed: None,
            finish: None,
            raw_finish: None,
            tokens: None,
            cost: None,
            error: None,
            open_text: None,
            open_reasoning: None,
        }
    }

    fn data(&self) -> Value {
        let mut time = Map::new();
        time.insert("created".to_owned(), Value::from(self.created));
        if let Some(streamed) = self.streamed {
            time.insert("streamed".to_owned(), Value::from(streamed));
        }
        if let Some(completed) = self.completed {
            time.insert("completed".to_owned(), Value::from(completed));
        }
        let mut map = Map::new();
        map.insert("time".to_owned(), Value::Object(time));
        map.insert("agent".to_owned(), Value::String(self.agent.clone()));
        map.insert(
            "model".to_owned(),
            serde_json::to_value(&self.model).expect("model ref is serializable"),
        );
        map.insert(
            "content".to_owned(),
            serde_json::to_value(&self.content).expect("assistant content is serializable"),
        );
        if let Some(finish) = self.finish {
            map.insert(
                "finish".to_owned(),
                serde_json::to_value(finish).expect("finish reason is serializable"),
            );
        }
        if let Some(raw) = &self.raw_finish {
            map.insert("rawFinish".to_owned(), Value::String(raw.clone()));
        }
        if let Some(tokens) = &self.tokens {
            map.insert(
                "tokens".to_owned(),
                serde_json::to_value(tokens).expect("tokens are serializable"),
            );
        }
        if let Some(cost) = self.cost {
            map.insert("cost".to_owned(), Value::from(cost));
        }
        if let Some(error) = &self.error {
            map.insert(
                "error".to_owned(),
                serde_json::to_value(error).expect("structured error is serializable"),
            );
        }
        Value::Object(map)
    }

    fn ensure_text(&mut self) -> (usize, bool) {
        if let Some(index) = self.open_text {
            return (index, false);
        }
        let index = self.content.len();
        self.content.push(AssistantPart::Text(TextPart {
            text: String::new(),
            native: None,
        }));
        self.open_text = Some(index);
        (index, true)
    }

    fn ensure_reasoning(&mut self) -> (usize, bool) {
        if let Some(index) = self.open_reasoning {
            return (index, false);
        }
        let index = self.content.len();
        self.content.push(AssistantPart::Reasoning(ReasoningPart {
            text: String::new(),
            native: None,
            time: None,
        }));
        self.open_reasoning = Some(index);
        (index, true)
    }

    fn append_text(&mut self, delta: &str) {
        let (index, _) = self.ensure_text();
        if let Some(AssistantPart::Text(part)) = self.content.get_mut(index) {
            part.text.push_str(delta);
        }
    }

    fn append_reasoning(&mut self, delta: &str) {
        let (index, _) = self.ensure_reasoning();
        if let Some(AssistantPart::Reasoning(part)) = self.content.get_mut(index) {
            part.text.push_str(delta);
        }
    }

    fn close_text(&mut self) -> Option<(usize, String)> {
        let index = self.open_text.take()?;
        let text = match self.content.get(index) {
            Some(AssistantPart::Text(part)) => part.text.clone(),
            _ => String::new(),
        };
        Some((index, text))
    }

    fn close_reasoning(&mut self) -> Option<(usize, String)> {
        let index = self.open_reasoning.take()?;
        let text = match self.content.get(index) {
            Some(AssistantPart::Reasoning(part)) => part.text.clone(),
            _ => String::new(),
        };
        if let Some(AssistantPart::Reasoning(part)) = self.content.get_mut(index) {
            part.time = Some(oc_types::message::CompletedTime {
                created: self.created,
                completed: Some(oc_types::now_millis()),
            });
        }
        Some((index, text))
    }

    fn tool_index(&self, id: &str) -> Option<usize> {
        self.content
            .iter()
            .position(|part| matches!(part, AssistantPart::Tool(tool) if tool.id == id))
    }

    fn ensure_tool(&mut self, id: &str, name: &str) -> usize {
        if let Some(index) = self.tool_index(id) {
            return index;
        }
        let index = self.content.len();
        self.content.push(AssistantPart::Tool(Box::new(ToolPart {
            id: id.to_owned(),
            name: name.to_owned(),
            executed: Some(false),
            native: None,
            state: ToolState::Streaming {
                input: String::new(),
            },
            time: ToolTime {
                created: oc_types::now_millis(),
                ran: None,
                completed: None,
            },
        })));
        index
    }

    fn append_tool_input(&mut self, id: &str, delta: &str) {
        let index = self.ensure_tool(id, "");
        if let Some(AssistantPart::Tool(tool)) = self.content.get_mut(index)
            && let ToolState::Streaming { input } = &mut tool.state
        {
            input.push_str(delta);
        }
    }

    fn set_tool_streamed(&mut self, id: &str, text: String) {
        let index = self.ensure_tool(id, "");
        if let Some(AssistantPart::Tool(tool)) = self.content.get_mut(index)
            && let ToolState::Streaming { input } = &mut tool.state
        {
            *input = text;
        }
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_execution(
    runtime: &Runtime,
    session: &str,
    item: &Value,
    cancel: &Arc<CancelToken>,
    handle: &Arc<ActorHandle>,
) -> Result<(), RuntimeError> {
    let info_value = runtime
        .inner
        .config
        .store
        .get_session(session)
        .await?
        .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
    let info = Runtime::parse_session(&info_value)?;
    let directory = info.location.directory.clone();
    let Some(inbox_id) = item.get("id").and_then(Value::as_str) else {
        return Ok(());
    };

    // Some hosts materialise the user row themselves before notifying. When the
    // row exists, the inbox entry is only a doorbell: consume it instead of
    // failing on the primary-key conflict.
    let promoted = if runtime
        .inner
        .config
        .store
        .get_message(session, inbox_id)
        .await?
        .is_some()
    {
        runtime
            .inner
            .config
            .store
            .cancel_inbox(session, inbox_id)
            .await?;
        Some(Value::Null)
    } else {
        match runtime
            .inner
            .config
            .store
            .promote_inbox(session, inbox_id)
            .await
        {
            Ok(promoted) => promoted,
            Err(error) => {
                if runtime
                    .inner
                    .config
                    .store
                    .get_message(session, inbox_id)
                    .await?
                    .is_some()
                {
                    runtime
                        .inner
                        .config
                        .store
                        .cancel_inbox(session, inbox_id)
                        .await?;
                    Some(Value::Null)
                } else {
                    return Err(error.into());
                }
            }
        }
    };
    if promoted.is_none() {
        return Ok(());
    }
    runtime
        .emit(
            session,
            &directory,
            SessionInboxDelivered {
                session_id: Runtime::session_id(session),
                inbox_id: MessageId::from_wire(inbox_id.to_owned()),
            },
        )
        .await?;
    runtime
        .emit(
            session,
            &directory,
            SessionStatusUpdated {
                session_id: Runtime::session_id(session),
                status: SessionStatus::Busy,
            },
        )
        .await?;
    runtime
        .emit(
            session,
            &directory,
            oc_types::payload::SessionExecutionStarted {
                session_id: Runtime::session_id(session),
            },
        )
        .await?;

    let mut turn = match runtime.resolve_turn(&info, &directory) {
        Ok(turn) => turn,
        Err(error) => {
            runtime
                .settle(
                    session,
                    &directory,
                    ExecutionOutcome::Failed(error.structured()),
                )
                .await?;
            return Ok(());
        }
    };
    if let Err(error) = runtime
        .refresh_instructions(session, &mut turn, &info, handle)
        .await
    {
        runtime
            .settle(
                session,
                &directory,
                ExecutionOutcome::Failed(error.structured()),
            )
            .await?;
        return Ok(());
    }

    let mut stats = TurnStats::default();
    let outcome = runtime
        .run_steps(session, &turn, cancel, &mut stats)
        .await?;
    if matches!(outcome, ExecutionOutcome::Succeeded) {
        runtime.touch_memory(&turn.memory_ids).await;
    }
    let succeeded = matches!(outcome, ExecutionOutcome::Succeeded);
    runtime.settle(session, &directory, outcome).await?;
    if succeeded {
        runtime.spawn_scribe(session, &stats).await;
    }
    Ok(())
}

impl Runtime {
    /// Stored credential for `provider`, preferring the integration's active
    /// row. Returns the tagged value's `key`, or an OAuth token as fallback.
    /// Store failures are logged; the client still falls back to the provider's
    /// environment variables.
    async fn credential_key(&self, provider: &str) -> Option<String> {
        let credentials = match self.inner.config.store.list_credentials().await {
            Ok(credentials) => credentials,
            Err(error) => {
                tracing::warn!(%error, provider, "could not read stored credentials");
                return None;
            }
        };

        let mut fallback = None;
        for entry in credentials {
            if entry.get("integrationID").and_then(Value::as_str) != Some(provider) {
                continue;
            }
            let Some(value) = entry.get("value") else {
                continue;
            };
            let key = match value.get("type").and_then(Value::as_str) {
                Some("oauth") => value.get("access_token").or_else(|| value.get("key")),
                _ => value.get("key").or_else(|| value.get("access_token")),
            }
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty());
            let Some(key) = key else {
                continue;
            };
            if entry.get("active").and_then(Value::as_bool) == Some(true) {
                return Some(key.to_owned());
            }
            fallback.get_or_insert_with(|| key.to_owned());
        }
        fallback
    }

    fn resolve_turn(
        &self,
        info: &SessionInfo,
        directory: &str,
    ) -> Result<ResolvedTurn, RuntimeError> {
        let config = &self.inner.config;
        let agent_id = info
            .agent
            .clone()
            .unwrap_or_else(|| config.config.default_agent().to_owned());
        let agent = config
            .config
            .agent(&agent_id)
            .ok_or_else(|| RuntimeError::AgentNotFound(agent_id.clone()))?;

        let model_ref = info
            .model
            .clone()
            .or_else(|| agent.get("model").and_then(prompt::model_ref))
            .or_else(|| {
                config
                    .config
                    .model()
                    .and_then(|model| prompt::model_ref(&Value::String(model)))
            })
            .or_else(|| {
                config
                    .catalog
                    .models()
                    .into_iter()
                    .next()
                    .map(|model| ModelRef {
                        id: model.model_id,
                        provider_id: model.provider_id,
                        variant: None,
                    })
            })
            .ok_or_else(|| RuntimeError::ModelUnavailable("no model configured".to_owned()))?;
        let resolved = config.catalog.resolve(&model_ref.to_reference())?;
        let context_window = resolved.model.limit.context;
        let session_rules = session_rules(info);

        let registry = {
            let mut merged = ToolRegistry::new();
            merged.extend(&config.tools);
            for tool in self.provider_tools(info, &agent_id) {
                merged.register(tool);
            }
            if let Some(memory) = self.memory() {
                let memory_registry = MemoryTools::new(memory)
                    .with_project_id(info.project_id.to_string())
                    .with_agent_id(agent_id.clone())
                    .registry();
                merged.extend(&memory_registry);
            }
            merged.extend(&crate::subagent::SubagentTools::new(self.clone()).registry());
            merged.extend(&crate::goal::GoalTools::new(self.clone()).registry());
            merged.extend(&crate::skill::SkillTools::new(self.clone()).registry());
            // LaneTools replaces the built-in `shell` with the lane-aware one.
            merged.extend(&crate::lane::LaneTools::new(self.clone()).registry());
            Arc::new(merged)
        };

        let mut tools = Vec::new();
        let mut tool_names = Vec::new();
        for definition in registry.definitions() {
            let action = tool_action(&definition.name);
            if self.effect(&agent_id, &session_rules, action, "*") == ConfigEffect::Deny {
                continue;
            }
            let mut schema = definition.parameters;
            expectation::inject(&mut schema);
            tool_names.push(definition.name.clone());
            tools.push(ToolDef::new(
                definition.name,
                definition.description,
                schema,
            ));
        }

        let agent_system = agent
            .get("system")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let max_steps = agent
            .get("steps")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_STEPS)
            .max(1);
        let max_steps = usize::try_from(max_steps).unwrap_or(usize::MAX);

        Ok(ResolvedTurn {
            info: info.clone(),
            directory: directory.to_owned(),
            agent_id,
            model_ref,
            model: resolved,
            agent_system,
            tool_names,
            registry,
            system: Vec::new(),
            tools,
            memory_ids: Vec::new(),
            max_steps,
            session_rules,
            context_window,
        })
    }

    /// Resolve the cache-stable instruction prefix and publish a delta when it
    /// changed. Runs once per execution, before the first provider call.
    async fn refresh_instructions(
        &self,
        session: &str,
        turn: &mut ResolvedTurn,
        info: &SessionInfo,
        handle: &Arc<ActorHandle>,
    ) -> Result<(), RuntimeError> {
        let node = self.inner.config.store.session_node(session).await?;
        let depth = node.as_ref().map_or(0, |node| node.depth);
        let root = node
            .as_ref()
            .map_or_else(|| session.to_owned(), |node| node.root_id.clone());
        let goal_value = self.inner.config.store.get_goal(&root).await?;
        let goal = goal_value.as_ref().map(crate::goal_store::snapshot);
        let stored = self.list_all_messages(session).await?;
        let skill_ids = loaded_skill_ids(&stored);
        let context = self.load_context(session).await?;
        let query = prompt::last_user_text(&context);
        let memory = self.memory();
        let assembly = prompt::assemble_instructions(&prompt::InstructionContext {
            config: &self.inner.config.config,
            memory: memory.as_deref(),
            agent_id: &turn.agent_id,
            project_id: info.project_id.as_ref(),
            session_id: session,
            depth,
            query: &query,
            skill_ids: &skill_ids,
            directory: &turn.directory,
            goal: goal.as_ref(),
        })
        .await;

        let (initial, delta) = {
            let mut guard = handle.instructions.lock().expect("instruction set");
            if let Some(set) = guard.as_mut() {
                let delta = set.commit(&assembly.sources);
                (set.initial().to_owned(), Some(delta))
            } else {
                let set = InstructionSet::new(&assembly.sources);
                let initial = set.initial().to_owned();
                *guard = Some(set);
                (initial, None)
            }
        };
        if let Some(delta) = delta
            && !delta.delta.is_empty()
        {
            self.emit(
                session,
                &turn.directory,
                SessionInstructionsUpdated {
                    session_id: session.to_owned(),
                    delta: delta.delta,
                    text: delta.text,
                },
            )
            .await?;
        }

        let has_memory = !assembly.memory_ids.is_empty();
        turn.memory_ids = assembly.memory_ids;
        turn.system = prompt::system_prefix(
            turn.agent_system.as_deref(),
            &turn.tool_names,
            &turn.directory,
            has_memory,
            &initial,
        );
        Ok(())
    }

    /// Bump `uses`/`last_used` for the memories injected into this turn.
    async fn touch_memory(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let Some(memory) = self.memory() else {
            return;
        };
        if let Err(error) = memory.touch(ids).await {
            tracing::warn!(error = %error, "memory touch failed");
        }
    }

    /// Run the scribe off the turn task. Failures are logged, never surfaced to
    /// the user or the execution outcome.
    ///
    /// `memory.auto_scribe` gates this turn-end pass only; the `digest` tool
    /// keeps working from the same scribe when extraction is turned off.
    async fn spawn_scribe(&self, session: &str, stats: &TurnStats) {
        let auto_scribe = self
            .inner
            .config
            .config
            .merged()
            .get("memory")
            .and_then(|memory| memory.get("auto_scribe"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !auto_scribe {
            return;
        }
        let (Some(scribe), Some(memory)) = (self.scribe_model(), self.memory()) else {
            return;
        };
        if stats.tool_calls == 0 && !self.has_goal(session).await {
            return;
        }
        let digest = scribe_digest(stats);
        let session = session.to_owned();
        tokio::spawn(async move {
            match oc_memory::run_scribe(scribe.as_ref(), &digest).await {
                Ok(candidates) => {
                    for candidate in candidates {
                        let written = memory
                            .write(NewMemoryInput {
                                text: candidate.text,
                                kind: Some(candidate.kind),
                                scope: Some("session".to_owned()),
                                scope_id: Some(session.clone()),
                                tags: candidate.tags,
                                importance: Some(candidate.importance),
                                confidence: Some(candidate.confidence),
                                source: Some("scribe".to_owned()),
                                source_session: Some(session.clone()),
                                source_message: None,
                            })
                            .await;
                        if let Err(error) = written {
                            tracing::warn!(
                                session = %session,
                                error = %error,
                                "scribe memory write failed"
                            );
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session = %session,
                        error = %error,
                        "scribe run failed; the turn is unaffected"
                    );
                }
            }
        });
    }

    async fn has_goal(&self, session: &str) -> bool {
        let Ok(Some(node)) = self.inner.config.store.session_node(session).await else {
            return false;
        };
        match self.inner.config.store.get_goal(&node.root_id).await {
            Ok(goal) => goal.is_some(),
            Err(error) => {
                tracing::debug!(error = %error, "goal lookup for the scribe failed");
                false
            }
        }
    }

    fn effect(
        &self,
        agent: &str,
        session_rules: &[PermissionRule],
        action: &str,
        resource: &str,
    ) -> ConfigEffect {
        let mut effect = self
            .inner
            .config
            .config
            .evaluate(Some(agent), action, resource);
        for rule in session_rules {
            if oc_config::wildcard_match(action, &rule.action)
                && oc_config::wildcard_match(resource, &rule.resource)
            {
                effect = rule.effect;
            }
        }
        effect
    }

    /// Promotes the pending `steer` rows that can become conversation
    /// messages and publishes their delivery event.
    ///
    /// This is the mid-turn counterpart of the promotion at the top of
    /// [`run_execution`]: it follows the same durable ordering (promote, then
    /// publish `session.inbox.delivered`, then let the caller read the
    /// message) and the same conflict handling for hosts that materialised the
    /// message themselves. `queue` rows and control kinds (`compaction`,
    /// `move`) are left for the next execution. Returns whether the context
    /// must be reloaded.
    async fn promote_steers(&self, session: &str, directory: &str) -> Result<bool, RuntimeError> {
        let store = &self.inner.config.store;
        let items = store.list_inbox(session).await?;
        let mut changed = false;
        for item in items {
            if item.get("delivery").and_then(Value::as_str) != Some("steer") {
                continue;
            }
            if !matches!(
                item.get("type").and_then(Value::as_str),
                Some("user" | "synthetic" | "peer" | "observer")
            ) {
                continue;
            }
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            // A host may have written the message before notifying; the row is
            // then only a doorbell and is consumed instead of conflicting.
            let promoted = if store.get_message(session, id).await?.is_some() {
                store.cancel_inbox(session, id).await?;
                Some(Value::Null)
            } else {
                match store.promote_inbox(session, id).await {
                    Ok(promoted) => promoted,
                    Err(error) => {
                        if store.get_message(session, id).await?.is_some() {
                            store.cancel_inbox(session, id).await?;
                            Some(Value::Null)
                        } else {
                            return Err(error.into());
                        }
                    }
                }
            };
            if promoted.is_none() {
                continue;
            }
            // The row is durable now; publish only after the promote commits.
            self.emit(
                session,
                directory,
                SessionInboxDelivered {
                    session_id: Runtime::session_id(session),
                    inbox_id: MessageId::from_wire(id.to_owned()),
                },
            )
            .await?;
            changed = true;
        }
        Ok(changed)
    }

    #[allow(clippy::too_many_lines)]
    async fn run_steps(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        cancel: &Arc<CancelToken>,
        stats: &mut TurnStats,
    ) -> Result<ExecutionOutcome, RuntimeError> {
        let mut context = self.load_context(session).await?;
        stats.user_text = prompt::last_user_text(&context);
        let mut compacted = false;
        let mut step_number = 0_usize;
        let ceiling = if turn.context_window == 0 {
            u64::MAX
        } else {
            tokens::compaction_ceiling(turn.context_window)
        };

        loop {
            if cancel.is_cancelled() {
                return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
            }
            // Steer rows that arrived while the previous step ran join the
            // conversation before this request is built. `queue` rows stay in
            // the inbox; the actor promotes them as the next execution.
            //
            // Rows still pending before the first request belong to the batch
            // admitted ahead of this execution (the execution promoted one of
            // them already), so they keep their own turn. Only rows that
            // arrived during the turn are promoted mid-turn.
            if step_number > 0 && self.promote_steers(session, &turn.directory).await? {
                context = self.load_context(session).await?;
            }
            if !compacted {
                let estimate = oc_llm::estimate_request_tokens(&turn.system, &context, &turn.tools);
                if estimate > ceiling {
                    match self.compact(session, turn, &context, cancel, "auto").await {
                        Ok(()) => context = self.load_context(session).await?,
                        Err(RuntimeError::Cancelled) => {
                            return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                        }
                        Err(error) => {
                            return Ok(ExecutionOutcome::Failed(error.structured()));
                        }
                    }
                }
                compacted = true;
            }

            let final_call = step_number >= turn.max_steps;
            let mut messages = context.clone();
            if final_call {
                messages.push(LlmMessage::user(MAX_STEPS_PROMPT));
            }
            let mut step = match self
                .stream_step(session, turn, &messages, final_call, cancel)
                .await
            {
                Ok(step) => step,
                Err(RuntimeError::Cancelled) => {
                    return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                }
                Err(error) => {
                    return Ok(ExecutionOutcome::Failed(error.structured()));
                }
            };

            if !final_call && !step.calls.is_empty() {
                stats.tool_calls = stats.tool_calls.saturating_add(step.calls.len());
                match self
                    .execute_tools(session, turn, &mut step.state, &step.calls, cancel)
                    .await
                {
                    Ok(()) => {}
                    Err(RuntimeError::Cancelled) => {
                        return Ok(ExecutionOutcome::Interrupted(cancel.reason()));
                    }
                    Err(error) => return Err(error),
                }
            }
            self.finish_step(session, turn, &mut step).await?;
            collect_tool_lines(stats, &step.state);
            if final_call || step.calls.is_empty() {
                stats.assistant_text = assistant_text(&step.state);
                return Ok(ExecutionOutcome::Succeeded);
            }
            step_number += 1;
            context = self.load_context(session).await?;
        }
    }

    async fn settle(
        &self,
        session: &str,
        directory: &str,
        outcome: ExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        match &outcome {
            ExecutionOutcome::Failed(_) => self.note_observer_execution(session, true).await,
            ExecutionOutcome::Interrupted(_) => self.note_observer_execution(session, false).await,
            ExecutionOutcome::Succeeded => {}
        }
        let now = oc_types::now_millis();
        let (name, error) = match &outcome {
            ExecutionOutcome::Succeeded => ("succeeded", None),
            ExecutionOutcome::Failed(error) => ("failed", Some(error.clone())),
            ExecutionOutcome::Interrupted(_) => ("interrupted", None),
        };
        let mut idle = Map::new();
        idle.insert("time".to_owned(), json!({ "created": now }));
        idle.insert("outcome".to_owned(), Value::String(name.to_owned()));
        if let Some(error) = &error {
            idle.insert(
                "error".to_owned(),
                serde_json::to_value(error).expect("structured error is serializable"),
            );
        }
        let idle_id = MessageId::new();
        self.inner
            .config
            .store
            .append_message(NewMessage {
                id: idle_id.as_str().to_owned(),
                session_id: session.to_owned(),
                kind: "idle".to_owned(),
                data: Value::Object(idle),
                time_created: now,
            })
            .await?;
        self.emit(
            session,
            directory,
            SessionIdle {
                session_id: Runtime::session_id(session),
            },
        )
        .await?;
        self.emit(
            session,
            directory,
            SessionStatusUpdated {
                session_id: Runtime::session_id(session),
                status: SessionStatus::Idle,
            },
        )
        .await?;
        match outcome {
            ExecutionOutcome::Succeeded => {
                self.emit(
                    session,
                    directory,
                    SessionExecutionSucceeded {
                        session_id: Runtime::session_id(session),
                    },
                )
                .await?;
            }
            ExecutionOutcome::Failed(error) => {
                self.emit(
                    session,
                    directory,
                    SessionExecutionFailed {
                        session_id: Runtime::session_id(session),
                        error,
                    },
                )
                .await?;
            }
            ExecutionOutcome::Interrupted(reason) => {
                self.emit(
                    session,
                    directory,
                    SessionExecutionInterrupted {
                        session_id: Runtime::session_id(session),
                        reason,
                    },
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn persist_assistant(
        &self,
        session: &str,
        state: &AssistantState,
    ) -> Result<(), RuntimeError> {
        self.inner
            .config
            .store
            .update_message(session, state.id.as_str(), state.data())
            .await?;
        Ok(())
    }

    async fn fail_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        mut state: AssistantState,
        error: StructuredError,
        tokens: Option<Tokens>,
        cost: f64,
    ) -> Result<(), RuntimeError> {
        state.error = Some(error.clone());
        state.finish = Some(FinishReason::Error);
        state.completed = Some(oc_types::now_millis());
        state.tokens = tokens.clone();
        state.cost = Some(cost);
        self.persist_assistant(session, &state).await?;
        self.emit(
            session,
            &turn.directory,
            SessionStepFailed {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                error,
                finish: Some(FinishReason::Error),
                raw_finish: state.raw_finish.clone(),
                native: None,
                cost: Some(cost),
                tokens,
                snapshot: None,
                files: None,
            },
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn stream_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        messages: &[LlmMessage],
        final_call: bool,
        cancel: &Arc<CancelToken>,
    ) -> Result<StepRun, RuntimeError> {
        let created = oc_types::now_millis();
        let id = MessageId::new();
        let mut state = AssistantState::new(
            id.clone(),
            turn.agent_id.clone(),
            turn.model_ref.clone(),
            created,
        );
        self.inner
            .config
            .store
            .append_message(NewMessage {
                id: id.as_str().to_owned(),
                session_id: session.to_owned(),
                kind: "assistant".to_owned(),
                data: state.data(),
                time_created: created,
            })
            .await?;
        self.emit(
            session,
            &turn.directory,
            SessionStepStarted {
                session_id: Runtime::session_id(session),
                assistant_message_id: id.clone(),
                agent: turn.agent_id.clone(),
                model: turn.model_ref.clone(),
                snapshot: None,
                started: created,
            },
        )
        .await?;

        let options = LlmOptions {
            api_key: self.credential_key(&turn.model.provider.id).await,
            tool_choice: final_call.then(|| "none".to_owned()),
            ..LlmOptions::default()
        };
        let request = LlmRequest {
            model: turn.model.clone(),
            system: turn.system.clone(),
            messages: messages.to_vec(),
            tools: if final_call {
                Vec::new()
            } else {
                turn.tools.clone()
            },
            options,
        };

        let stream_result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.inner.model.stream(request) => Some(result),
        };
        let Some(stream_result) = stream_result else {
            self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                .await;
            return Err(RuntimeError::Cancelled);
        };
        let mut stream = match stream_result {
            Ok(stream) => stream,
            Err(error) => {
                let runtime_error = RuntimeError::Llm(error);
                self.fail_step(session, turn, state, runtime_error.structured(), None, 0.0)
                    .await?;
                return Err(runtime_error);
            }
        };

        let mut calls = Vec::new();
        let mut provider_error = None;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                        .await;
                    return Err(RuntimeError::Cancelled);
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                oc_llm::LlmEvent::StepStart => {}
                oc_llm::LlmEvent::TextStart => {
                    self.close_reasoning(session, turn, &mut state).await?;
                    self.mark_streamed(session, turn, &mut state).await?;
                    let (ordinal, created_part) = state.ensure_text();
                    if created_part {
                        self.emit(
                            session,
                            &turn.directory,
                            SessionTextStarted {
                                session_id: Runtime::session_id(session),
                                assistant_message_id: id.clone(),
                                ordinal: ordinal_u32(ordinal),
                            },
                        )
                        .await?;
                    }
                }
                oc_llm::LlmEvent::TextDelta { delta } => {
                    self.mark_streamed(session, turn, &mut state).await?;
                    state.append_text(&delta);
                    self.emit(
                        session,
                        &turn.directory,
                        SessionTextDelta {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            ordinal: ordinal_u32(state.open_text.unwrap_or(0)),
                            delta,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::TextEnd { text } => {
                    self.mark_streamed(session, turn, &mut state).await?;
                    let (ordinal, _) = state.ensure_text();
                    if let Some(AssistantPart::Text(part)) = state.content.get_mut(ordinal) {
                        part.text.clone_from(&text);
                    }
                    state.open_text = None;
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionTextEnded {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            ordinal: ordinal_u32(ordinal),
                            text,
                            native: None,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ReasoningDelta { delta } => {
                    self.mark_streamed(session, turn, &mut state).await?;
                    let (ordinal, created_part) = state.ensure_reasoning();
                    if created_part {
                        self.emit(
                            session,
                            &turn.directory,
                            oc_types::payload::SessionReasoningStarted {
                                session_id: Runtime::session_id(session),
                                assistant_message_id: id.clone(),
                                ordinal: ordinal_u32(ordinal),
                                native: None,
                            },
                        )
                        .await?;
                    }
                    state.append_reasoning(&delta);
                    self.emit(
                        session,
                        &turn.directory,
                        oc_types::payload::SessionReasoningDelta {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            ordinal: ordinal_u32(ordinal),
                            delta,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolInputStart { id: call_id, name } => {
                    if final_call {
                        continue;
                    }
                    self.close_reasoning(session, turn, &mut state).await?;
                    self.mark_streamed(session, turn, &mut state).await?;
                    state.ensure_tool(&call_id, &name);
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolInputStarted {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id,
                            name,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolInputDelta { id: call_id, delta } => {
                    if final_call {
                        continue;
                    }
                    self.mark_streamed(session, turn, &mut state).await?;
                    state.append_tool_input(&call_id, &delta);
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolInputDelta {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id,
                            delta,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolInputEnd { id: call_id, text } => {
                    if final_call {
                        continue;
                    }
                    state.set_tool_streamed(&call_id, text.clone());
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolInputEnded {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id,
                            text,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ToolCall {
                    id: call_id,
                    name,
                    input,
                } => {
                    if final_call {
                        continue;
                    }
                    self.close_reasoning(session, turn, &mut state).await?;
                    self.mark_streamed(session, turn, &mut state).await?;
                    let index = state.ensure_tool(&call_id, &name);
                    let (clean, expectation, invalid) = strip_expectation(&name, input);
                    if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index) {
                        tool.name.clone_from(&name);
                        tool.state = ToolState::Running {
                            input: clean.clone(),
                            metadata: None,
                        };
                        tool.executed = Some(false);
                    }
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionToolCalled {
                            session_id: Runtime::session_id(session),
                            assistant_message_id: id.clone(),
                            id: call_id.clone(),
                            input: clean.clone(),
                            executed: false,
                            native: None,
                        },
                    )
                    .await?;
                    calls.push(PendingToolCall {
                        index,
                        id: call_id,
                        name,
                        input: clean,
                        expectation,
                        invalid,
                    });
                }
                oc_llm::LlmEvent::Finish { reason, usage } => {
                    state.finish = Some(finish_reason(&reason));
                    state.raw_finish = Some(reason);
                    let tokens = tokens_from_usage(&usage);
                    state.tokens = Some(tokens.clone());
                    let cost = oc_llm::model_cost(&turn.model.model, &usage);
                    state.cost = Some(cost);
                    self.persist_assistant(session, &state).await?;
                    self.emit(
                        session,
                        &turn.directory,
                        SessionUsageUpdated {
                            session_id: Runtime::session_id(session),
                            cost,
                            tokens,
                        },
                    )
                    .await?;
                }
                oc_llm::LlmEvent::ProviderError { message } => {
                    provider_error = Some(message);
                    break;
                }
            }
        }

        self.close_reasoning(session, turn, &mut state).await?;
        self.close_text(session, turn, &mut state).await?;
        if let Some(message) = provider_error {
            let error = StructuredError::new("ProviderError", message.clone());
            let tokens = state.tokens.clone().unwrap_or_default();
            let cost = state.cost.unwrap_or(0.0);
            self.fail_step(session, turn, state, error, Some(tokens), cost)
                .await?;
            return Err(RuntimeError::Llm(oc_llm::LlmError::Provider { message }));
        }
        if cancel.is_cancelled() {
            self.schedule_finalizer(session.to_owned(), turn.directory.clone(), state)
                .await;
            return Err(RuntimeError::Cancelled);
        }
        state.finish.get_or_insert(FinishReason::Unknown);
        self.persist_assistant(session, &state).await?;
        Ok(StepRun {
            tokens: state.tokens.clone().unwrap_or_default(),
            cost: state.cost.unwrap_or(0.0),
            state,
            calls,
        })
    }

    async fn mark_streamed(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
    ) -> Result<(), RuntimeError> {
        if state.streamed.is_some() {
            return Ok(());
        }
        state.streamed = Some(oc_types::now_millis());
        self.emit(
            session,
            &turn.directory,
            SessionStepStreamed {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
            },
        )
        .await
    }

    async fn close_text(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
    ) -> Result<(), RuntimeError> {
        let Some((index, text)) = state.close_text() else {
            return Ok(());
        };
        self.persist_assistant(session, state).await?;
        self.emit(
            session,
            &turn.directory,
            SessionTextEnded {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                ordinal: ordinal_u32(index),
                text,
                native: None,
            },
        )
        .await
    }

    async fn close_reasoning(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
    ) -> Result<(), RuntimeError> {
        let Some((index, text)) = state.close_reasoning() else {
            return Ok(());
        };
        self.persist_assistant(session, state).await?;
        self.emit(
            session,
            &turn.directory,
            oc_types::payload::SessionReasoningEnded {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                ordinal: ordinal_u32(index),
                text,
                native: None,
            },
        )
        .await
    }

    async fn finish_step(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        step: &mut StepRun,
    ) -> Result<(), RuntimeError> {
        step.state.completed = Some(oc_types::now_millis());
        step.state.finish.get_or_insert(FinishReason::Unknown);
        let tokens = step
            .state
            .tokens
            .clone()
            .unwrap_or_else(|| step.tokens.clone());
        step.state.tokens = Some(tokens.clone());
        let cost = step.state.cost.unwrap_or(step.cost);
        step.state.cost = Some(cost);
        self.persist_assistant(session, &step.state).await?;
        self.emit(
            session,
            &turn.directory,
            SessionStepEnded {
                session_id: Runtime::session_id(session),
                assistant_message_id: step.state.id.clone(),
                finish: step.state.finish.unwrap_or(FinishReason::Unknown),
                raw_finish: step.state.raw_finish.clone(),
                native: None,
                cost,
                tokens,
                snapshot: None,
                files: None,
            },
        )
        .await
    }

    async fn execute_tools(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        calls: &[PendingToolCall],
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let observer = self.observer();
        let (root_id, depth) = if observer.is_some() {
            match self.inner.config.store.session_node(session).await {
                Ok(Some(node)) => (node.root_id, node.depth),
                Ok(None) => (session.to_owned(), 0),
                Err(error) => return Err(error.into()),
            }
        } else {
            (String::new(), 0)
        };
        let mut open: Vec<String> = Vec::new();
        let mut set = tokio::task::JoinSet::new();
        let mut next = 0_usize;
        while next < calls.len() || !set.is_empty() {
            while set.len() < MAX_TOOL_PARALLELISM && next < calls.len() {
                let call = calls[next].clone();
                let index = call.index;
                if let Some(engine) = observer.as_deref()
                    && let Some(strip) = crate::observer::flight_strip(
                        &root_id,
                        session,
                        &turn.agent_id,
                        depth,
                        &call,
                    )
                {
                    engine.observe_start(strip);
                    open.push(call.id.clone());
                }
                let runtime = self.clone();
                let turn = turn.clone();
                let message_id = state.id.clone();
                let token = Arc::clone(cancel);
                set.spawn(async move {
                    (
                        index,
                        run_tool(runtime, turn, message_id, call, token).await,
                    )
                });
                next += 1;
            }
            let joined = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    settle_open(observer.as_deref(), &mut open, "cancelled");
                    self.schedule_finalizer(
                        session.to_owned(),
                        turn.directory.clone(),
                        state.clone(),
                    )
                    .await;
                    return Err(RuntimeError::Cancelled);
                }
                joined = set.join_next() => joined,
            };
            match joined {
                Some(Ok((_index, ToolOutcome::Cancelled))) => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    settle_open(observer.as_deref(), &mut open, "cancelled");
                    self.schedule_finalizer(
                        session.to_owned(),
                        turn.directory.clone(),
                        state.clone(),
                    )
                    .await;
                    return Err(RuntimeError::Cancelled);
                }
                Some(Ok((index, outcome))) => {
                    if let Some(engine) = observer.as_deref()
                        && let Some(call) = calls.iter().find(|call| call.index == index)
                    {
                        let (strip_state, strip_outcome) = crate::observer::strip_result(&outcome);
                        engine.observe_settle(&call.id, strip_state, strip_outcome.as_deref());
                        let call_id = call.id.clone();
                        open.retain(|id| id != &call_id);
                    }
                    self.apply_tool_outcome(session, turn, state, outcome)
                        .await?;
                }
                Some(Err(error)) => {
                    settle_open(observer.as_deref(), &mut open, "task failed");
                    return Err(RuntimeError::Task(error.to_string()));
                }
                None => break,
            }
        }
        Ok(())
    }

    async fn apply_tool_outcome(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        state: &mut AssistantState,
        outcome: ToolOutcome,
    ) -> Result<(), RuntimeError> {
        let now = oc_types::now_millis();
        match outcome {
            ToolOutcome::Completed {
                index,
                input,
                content,
                metadata,
                executed,
            } => {
                if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index) {
                    tool.state = ToolState::Completed {
                        input: input.clone(),
                        content: content.clone(),
                        metadata: metadata.clone(),
                    };
                    tool.executed = Some(executed);
                    tool.time.ran = Some(now);
                    tool.time.completed = Some(now);
                }
                self.persist_assistant(session, state).await?;
                let id = tool_id(state, index);
                self.emit(
                    session,
                    &turn.directory,
                    SessionToolSuccess {
                        session_id: Runtime::session_id(session),
                        assistant_message_id: state.id.clone(),
                        id,
                        content,
                        metadata,
                        executed,
                        native: None,
                    },
                )
                .await?;
            }
            ToolOutcome::Failed {
                index,
                input,
                error,
                content,
                metadata,
                executed,
            } => {
                if let Some(AssistantPart::Tool(tool)) = state.content.get_mut(index) {
                    tool.state = ToolState::Error {
                        input,
                        error: error.clone(),
                        content: content.clone(),
                        metadata: metadata.clone(),
                    };
                    tool.executed = Some(executed);
                    if executed {
                        tool.time.ran = Some(now);
                    }
                    tool.time.completed = Some(now);
                }
                self.persist_assistant(session, state).await?;
                let id = tool_id(state, index);
                self.emit(
                    session,
                    &turn.directory,
                    SessionToolFailed {
                        session_id: Runtime::session_id(session),
                        assistant_message_id: state.id.clone(),
                        id,
                        error,
                        content,
                        metadata,
                        executed,
                        native: None,
                    },
                )
                .await?;
            }
            ToolOutcome::Cancelled => {}
        }
        Ok(())
    }

    async fn schedule_finalizer(&self, session: String, directory: String, state: AssistantState) {
        let runtime = self.clone();
        let handle = tokio::spawn(async move {
            runtime
                .finalize_interrupted(&session, &directory, state)
                .await
        });
        match tokio::time::timeout(Duration::from_secs(5), handle).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                tracing::error!(error = %error, "interrupt finalizer failed");
            }
            Ok(Err(error)) => {
                tracing::error!(error = %error, "interrupt finalizer task failed");
            }
            Err(_) => {
                tracing::error!("interrupt finalizer exceeded its five second budget");
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn finalize_interrupted(
        &self,
        session: &str,
        directory: &str,
        mut state: AssistantState,
    ) -> Result<(), RuntimeError> {
        if let Some((index, text)) = state.close_text() {
            self.emit(
                session,
                directory,
                SessionTextEnded {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    ordinal: ordinal_u32(index),
                    text,
                    native: None,
                },
            )
            .await?;
        }
        if let Some((index, text)) = state.close_reasoning() {
            self.emit(
                session,
                directory,
                oc_types::payload::SessionReasoningEnded {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    ordinal: ordinal_u32(index),
                    text,
                    native: None,
                },
            )
            .await?;
        }

        let tool_error = StructuredError::new(
            "ToolAbortedError",
            "Tool execution was cancelled before it completed.",
        );
        let now = oc_types::now_millis();
        let mut aborted = Vec::new();
        for part in &mut state.content {
            let AssistantPart::Tool(tool) = part else {
                continue;
            };
            if matches!(
                tool.state,
                ToolState::Streaming { .. } | ToolState::Running { .. }
            ) {
                let input = match &tool.state {
                    ToolState::Running { input, .. } => input.clone(),
                    _ => Map::new(),
                };
                tool.state = ToolState::Error {
                    input,
                    error: tool_error.clone(),
                    content: None,
                    metadata: None,
                };
                tool.executed = Some(false);
                tool.time.completed = Some(now);
                aborted.push(tool.id.clone());
            }
        }

        let error = StructuredError::new(
            "MessageAbortedError",
            "The operation was interrupted before it completed (STEP_INTERRUPTED).",
        );
        state.error = Some(error.clone());
        state.finish = Some(FinishReason::Error);
        state.completed = Some(now);
        self.persist_assistant(session, &state).await?;
        for id in aborted {
            self.emit(
                session,
                directory,
                SessionToolFailed {
                    session_id: Runtime::session_id(session),
                    assistant_message_id: state.id.clone(),
                    id,
                    error: tool_error.clone(),
                    content: None,
                    metadata: None,
                    executed: false,
                    native: None,
                },
            )
            .await?;
        }
        self.emit(
            session,
            directory,
            SessionStepFailed {
                session_id: Runtime::session_id(session),
                assistant_message_id: state.id.clone(),
                error,
                finish: Some(FinishReason::Error),
                raw_finish: state.raw_finish.clone(),
                native: None,
                cost: state.cost,
                tokens: state.tokens,
                snapshot: None,
                files: None,
            },
        )
        .await?;
        Ok(())
    }

    /// Runs one `compaction` inbox row: resolve the session turn, load the
    /// context and call [`Runtime::compact`].
    ///
    /// The inbox row is consumed after the attempt succeeds, fails or is
    /// cancelled, so a broken compaction cannot spin the actor. Turn-level
    /// failures are already persisted as a failed compaction message; they are
    /// logged here and reported as a clean outcome because the actor must keep
    /// serving the session.
    pub(crate) async fn run_manual_compaction(
        &self,
        session: &str,
        item: &Value,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let outcome = self.compact_from_inbox(session, cancel).await;
        if let Some(inbox_id) = item.get("id").and_then(Value::as_str) {
            // Consume the row before reporting success. If the delete fails the
            // row would still be the next deliverable and the actor would run
            // the same compaction again without backoff, so surface the error
            // instead of pretending the row is gone.
            self.inner
                .config
                .store
                .cancel_inbox(session, inbox_id)
                .await?;
        }
        match outcome {
            Ok(()) => {
                tracing::debug!(session = %session, "manual compaction completed");
            }
            Err(RuntimeError::Cancelled) => {
                tracing::debug!(session = %session, "manual compaction cancelled");
            }
            Err(error) => {
                tracing::warn!(
                    session = %session,
                    error = %error,
                    "manual compaction failed"
                );
            }
        }
        Ok(())
    }

    async fn compact_from_inbox(
        &self,
        session: &str,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), RuntimeError> {
        let info_value = self
            .inner
            .config
            .store
            .get_session(session)
            .await?
            .ok_or_else(|| RuntimeError::SessionNotFound(session.to_owned()))?;
        let info = Runtime::parse_session(&info_value)?;
        let directory = info.location.directory.clone();
        let turn = self.resolve_turn(&info, &directory)?;
        let context = self.load_context(session).await?;
        self.compact(session, &turn, &context, cancel, "manual")
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn compact(
        &self,
        session: &str,
        turn: &ResolvedTurn,
        context: &[LlmMessage],
        cancel: &Arc<CancelToken>,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        let created = oc_types::now_millis();
        let id = MessageId::new();
        let keep_tokens = self.compaction_keep_tokens();
        let keep_id = self.compaction_keep_id(session, keep_tokens).await?;
        let mut running = json!({
            "time": {"created": created},
            "status": "running",
            "reason": reason,
            "summary": ""
        });
        if let Some(keep_id) = &keep_id {
            running["keepID"] = json!(keep_id);
        }
        self.inner
            .config
            .store
            .append_message(NewMessage {
                id: id.as_str().to_owned(),
                session_id: session.to_owned(),
                kind: "compaction".to_owned(),
                data: running,
                time_created: created,
            })
            .await?;

        let mut messages = context.to_vec();
        messages.push(LlmMessage::user(crate::compaction::summary_prompt(None)));
        let api_key = self.credential_key(&turn.model.provider.id).await;
        let request = LlmRequest {
            model: turn.model.clone(),
            system: vec![
                "You compact an agent conversation into a handoff summary. Follow the \
                 requested sections exactly and keep concrete facts, paths and failures."
                    .to_owned(),
            ],
            messages,
            tools: Vec::new(),
            options: LlmOptions {
                api_key,
                tool_choice: Some("none".to_owned()),
                ..LlmOptions::default()
            },
        };

        let stream_result = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            result = self.inner.model.stream(request) => Some(result),
        };
        let Some(stream_result) = stream_result else {
            self.fail_compaction(session, &id, created, reason, "compaction was cancelled")
                .await?;
            return Err(RuntimeError::Cancelled);
        };
        let mut stream = match stream_result {
            Ok(stream) => stream,
            Err(error) => {
                self.fail_compaction(session, &id, created, reason, &error.to_string())
                    .await?;
                return Err(RuntimeError::Llm(error));
            }
        };

        let mut summary = String::new();
        let mut failure = None;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.fail_compaction(session, &id, created, reason, "compaction was cancelled")
                        .await?;
                    return Err(RuntimeError::Cancelled);
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                oc_llm::LlmEvent::TextDelta { delta } => summary.push_str(&delta),
                oc_llm::LlmEvent::TextEnd { text } => summary = text,
                oc_llm::LlmEvent::ProviderError { message } => {
                    failure = Some(message);
                    break;
                }
                _ => {}
            }
        }
        if let Some(message) = failure {
            self.fail_compaction(session, &id, created, reason, &message)
                .await?;
            return Err(RuntimeError::Llm(oc_llm::LlmError::Provider { message }));
        }
        if summary.trim().is_empty() {
            self.fail_compaction(session, &id, created, reason, "the summary was empty")
                .await?;
            return Err(RuntimeError::Config(
                "compaction returned an empty summary".to_owned(),
            ));
        }
        let mut completed = json!({
            "time": {"created": created},
            "status": "completed",
            "reason": reason,
            "model": turn.model_ref.clone(),
            "summary": summary
        });
        if let Some(keep_id) = &keep_id {
            completed["keepID"] = json!(keep_id);
        }
        self.inner
            .config
            .store
            .update_message(session, id.as_str(), completed)
            .await?;
        Ok(())
    }

    /// Recent-context budget preserved across a compaction (`compaction.keep.tokens`).
    fn compaction_keep_tokens(&self) -> u64 {
        self.inner
            .config
            .config
            .merged()
            .get("compaction")
            .and_then(|value| value.get("keep"))
            .and_then(|value| value.get("tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(crate::compaction::CompactionConfig::default().keep_tokens)
    }

    /// Oldest stored message to keep verbatim after a compaction.
    ///
    /// The summary replaces older history; the tail keeps the active turn (or
    /// the most recent `keep_tokens` budget) so the next request does not lose
    /// the prompt that triggered compaction. `None` means "summary only".
    async fn compaction_keep_id(
        &self,
        session: &str,
        keep_tokens: u64,
    ) -> Result<Option<String>, RuntimeError> {
        if keep_tokens == 0 {
            return Ok(None);
        }
        let messages = self.list_all_messages(session).await?;
        let boundary = messages.iter().rposition(|message| {
            message.get("type").and_then(Value::as_str) == Some("compaction")
                && message.get("status").and_then(Value::as_str) == Some("completed")
        });
        let start = boundary.map_or(0, |index| index + 1);
        let candidates: Vec<&Value> = messages[start..]
            .iter()
            .filter(|message| {
                matches!(
                    message.get("type").and_then(Value::as_str),
                    Some("user" | "synthetic" | "system" | "skill" | "peer" | "assistant")
                )
            })
            .collect();
        if candidates.is_empty() {
            return Ok(None);
        }
        let costs: Vec<u64> = candidates
            .iter()
            .map(|message| {
                let converted = crate::prompt::messages_from_store(std::slice::from_ref(*message));
                oc_llm::estimate_messages_tokens(&converted)
            })
            .collect();
        let total: u64 = costs.iter().sum();
        let mut keep_start = candidates.len();
        if total <= keep_tokens {
            // Keep the active turn: the last user/peer message and everything
            // after it. Keeping the entire small history would duplicate the
            // summary without reducing the prompt.
            keep_start = candidates
                .iter()
                .rposition(|message| {
                    matches!(
                        message.get("type").and_then(Value::as_str),
                        Some("user" | "synthetic" | "peer")
                    )
                })
                .unwrap_or(candidates.len().saturating_sub(1));
        } else {
            let mut accumulated = 0_u64;
            for index in (0..candidates.len()).rev() {
                let next = accumulated.saturating_add(costs[index]);
                if next > keep_tokens && accumulated > 0 {
                    break;
                }
                accumulated = next;
                keep_start = index;
                if accumulated >= keep_tokens {
                    break;
                }
            }
        }
        Ok(candidates
            .get(keep_start)
            .and_then(|message| message.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    async fn fail_compaction(
        &self,
        session: &str,
        id: &MessageId,
        created: i64,
        reason: &str,
        message: &str,
    ) -> Result<(), RuntimeError> {
        let failed = json!({
            "time": {"created": created},
            "status": "failed",
            "reason": reason,
            "error": {
                "type": "CompactionError",
                "message": message
            }
        });
        self.inner
            .config
            .store
            .update_message(session, id.as_str(), failed)
            .await?;
        Ok(())
    }
}

async fn run_tool(
    runtime: Runtime,
    turn: ResolvedTurn,
    message_id: MessageId,
    call: PendingToolCall,
    cancel: Arc<CancelToken>,
) -> ToolOutcome {
    if let Some(expectation) = &call.expectation {
        tracing::debug!(
            tool = %call.name,
            why = %expectation.why,
            expect_seconds = expectation.expect_seconds,
            "tool expectation"
        );
    }
    if let Some(error) = call.invalid.clone() {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error,
            content: None,
            metadata: None,
            executed: false,
        };
    }

    if let Err(failure) = authorize_tool_call(&runtime, &turn, &message_id, &call, &cancel).await {
        return auth_outcome(failure, call);
    }

    let Some(tool) = turn.registry.get(&call.name).cloned() else {
        return ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new(
                "ToolNotFoundError",
                format!("Tool {:?} is not registered.", call.name),
            ),
            content: None,
            metadata: None,
            executed: false,
        };
    };

    let context = ToolContext {
        session_id: turn.info.id.to_string(),
        directory: Path::new(&turn.directory).to_path_buf(),
        cwd: Path::new(&turn.directory).to_path_buf(),
        cancel: Some(cancel.token.clone()),
    };
    let input = Value::Object(call.input.clone());
    crate::snapshot::capture_before_tool(
        &runtime,
        turn.info.id.as_ref(),
        message_id.as_ref(),
        &call.name,
        &call.id,
        &input,
        &turn.directory,
    )
    .await;
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return ToolOutcome::Cancelled,
        result = tool.execute(&context, input) => result,
    };
    match result {
        Ok(result) => {
            let (tool_parts, tool_metadata) = tool_content(&result);
            ToolOutcome::Completed {
                index: call.index,
                input: call.input,
                content: tool_parts,
                metadata: tool_metadata,
                executed: true,
            }
        }
        Err(error) => ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error: StructuredError::new("ToolError", error.message),
            content: None,
            metadata: metadata_map(error.metadata.as_ref()),
            executed: true,
        },
    }
}

/// Authorize one dispatched tool call.
///
/// `patch` is special-cased because its input carries paths only inside the
/// patch text; [`authorize_patch`] checks every target. Every other tool uses
/// the single `permission_target` resource.
async fn authorize_tool_call(
    runtime: &Runtime,
    turn: &ResolvedTurn,
    message_id: &MessageId,
    call: &PendingToolCall,
    cancel: &Arc<CancelToken>,
) -> Result<(), AuthFailure> {
    if call.name == "patch" {
        return authorize_patch(runtime, turn, message_id, call, cancel).await;
    }
    let (action, resource, external) = permission_target(&call.name, &call.input, &turn.directory);
    if let Some(path) = external {
        runtime
            .authorize(
                turn,
                message_id,
                &call.name,
                &call.id,
                "external_directory",
                &path,
                cancel,
            )
            .await?;
    }
    runtime
        .authorize(
            turn, message_id, &call.name, &call.id, &action, &resource, cancel,
        )
        .await
}

/// Authorize every path a `patch` call touches.
///
/// `oc-tools` parses the patch envelope (the tool rejects invalid text before
/// applying anything), so this sees the same Add/Delete/Update/Move paths the
/// tool will write. Each target is checked as `edit` with the same
/// normalization and `external_directory` rule used by `write`/`edit`.
async fn authorize_patch(
    runtime: &Runtime,
    turn: &ResolvedTurn,
    message_id: &MessageId,
    call: &PendingToolCall,
    cancel: &Arc<CancelToken>,
) -> Result<(), AuthFailure> {
    let patch_text = call
        .input
        .get("patchText")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for target in oc_tools::patch::patch_targets(patch_text) {
        let path = normalize_path(&turn.directory, &target);
        if !inside(&turn.directory, &path) {
            runtime
                .authorize(
                    turn,
                    message_id,
                    &call.name,
                    &call.id,
                    "external_directory",
                    &path,
                    cancel,
                )
                .await?;
        }
        runtime
            .authorize(
                turn, message_id, &call.name, &call.id, "edit", &path, cancel,
            )
            .await?;
    }
    Ok(())
}

fn settle_open(
    engine: Option<&oc_observer::ObserverEngine>,
    open: &mut Vec<String>,
    outcome: &str,
) {
    let Some(engine) = engine else {
        open.clear();
        return;
    };
    for id in open.drain(..) {
        engine.observe_settle(&id, oc_observer::StripState::Cancelled, Some(outcome));
    }
}

fn auth_outcome(failure: AuthFailure, call: PendingToolCall) -> ToolOutcome {
    match failure {
        AuthFailure::Cancelled => ToolOutcome::Cancelled,
        AuthFailure::Denied(error) => ToolOutcome::Failed {
            index: call.index,
            input: call.input,
            error,
            content: None,
            metadata: None,
            executed: false,
        },
    }
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    async fn authorize(
        &self,
        turn: &ResolvedTurn,
        message_id: &MessageId,
        tool: &str,
        call_id: &str,
        action: &str,
        resource: &str,
        cancel: &Arc<CancelToken>,
    ) -> Result<(), AuthFailure> {
        match self.effect(&turn.agent_id, &turn.session_rules, action, resource) {
            ConfigEffect::Allow => Ok(()),
            ConfigEffect::Deny => Err(AuthFailure::Denied(StructuredError::new(
                "PermissionDeniedError",
                format!("Permission denied for {action} on {resource}."),
            ))),
            ConfigEffect::Ask => {
                let ask = PermissionAsk {
                    session_id: turn.info.id.to_string(),
                    message_id: message_id.to_string(),
                    tool_call_id: call_id.to_owned(),
                    tool: tool.to_owned(),
                    action: action.to_owned(),
                    resources: vec![resource.to_owned()],
                    message: Some(format!("Allow {action} on {resource}?")),
                };
                let decision = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(AuthFailure::Cancelled),
                    decision = self.inner.config.permissions.resolve(ask) => decision,
                };
                match decision {
                    PermissionDecision::Allow => Ok(()),
                    PermissionDecision::Deny { message } => {
                        let message = if message.trim().is_empty() {
                            format!("Permission denied for {action} on {resource}.")
                        } else {
                            message
                        };
                        Err(AuthFailure::Denied(StructuredError::new(
                            "PermissionDeniedError",
                            message,
                        )))
                    }
                }
            }
        }
    }
}

fn session_rules(info: &SessionInfo) -> Vec<PermissionRule> {
    info.permissions
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|rule| {
            let effect = match rule.effect {
                WireEffect::Allow => ConfigEffect::Allow,
                WireEffect::Deny => ConfigEffect::Deny,
                WireEffect::Ask => ConfigEffect::Ask,
            };
            PermissionRule::new(rule.action, rule.resource, effect)
        })
        .collect()
}

fn tool_action(name: &str) -> &str {
    match name {
        "write" | "edit" | "patch" => "edit",
        "subagent" => "subagent",
        "talk" | "session_write" | "roster" | "digest" | "memory_search" | "memory_list" => "read",
        "memory_write" | "memory_forget" => "memory:write",
        "goal_set" | "goal_update" | "goal_complete" | "goal_status" => "goal",
        "skill" | "skill_lint" => "skill",
        "skill_write" => "skill:write",
        "lane" => "lane",
        other => other,
    }
}

fn permission_target(
    tool: &str,
    input: &Map<String, Value>,
    directory: &str,
) -> (String, String, Option<String>) {
    let action = tool_action(tool).to_owned();
    match tool {
        "read" | "write" | "edit" | "patch" | "glob" | "grep" => {
            let raw = input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(directory);
            let path = normalize_path(directory, raw);
            let external = (!inside(directory, &path)).then(|| path.clone());
            (action, path, external)
        }
        "shell" => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (action, command.to_owned(), None)
        }
        "subagent" => {
            let agent = input.get("agent").and_then(Value::as_str).unwrap_or("*");
            (action, agent.to_owned(), None)
        }
        "talk" => {
            let to = input.get("to").and_then(Value::as_str).unwrap_or("*");
            (action, to.to_owned(), None)
        }
        "session_write" | "digest" => {
            let session = input
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("*");
            (action, session.to_owned(), None)
        }
        "skill" | "skill_write" | "skill_lint" => {
            let id = input.get("id").and_then(Value::as_str).unwrap_or("*");
            (action, id.to_owned(), None)
        }
        "lane" => {
            let resource = input
                .get("lane")
                .or_else(|| input.get("command"))
                .and_then(Value::as_str)
                .unwrap_or("*");
            (action, resource.to_owned(), None)
        }
        "goal_set" | "goal_update" | "goal_complete" | "goal_status" => {
            (action, "*".to_owned(), None)
        }
        _ => {
            let resource = input
                .get("path")
                .or_else(|| input.get("url"))
                .or_else(|| input.get("query"))
                .and_then(Value::as_str)
                .unwrap_or("*");
            (action, resource.to_owned(), None)
        }
    }
}

fn normalize_path(directory: &str, raw: &str) -> String {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(directory).join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.to_string_lossy().into_owned()
}

fn inside(directory: &str, path: &str) -> bool {
    let base = directory.replace('\\', "/").to_lowercase();
    let path = path.replace('\\', "/").to_lowercase();
    path == base || path.starts_with(&format!("{base}/"))
}

fn strip_expectation(
    tool: &str,
    input: Value,
) -> (
    Map<String, Value>,
    Option<Expectation>,
    Option<StructuredError>,
) {
    let mut value = input;
    match expectation::extract(&mut value, tool) {
        Ok(expectation) => (
            value.as_object().cloned().unwrap_or_default(),
            Some(expectation),
            None,
        ),
        Err(error) => (
            value.as_object().cloned().unwrap_or_default(),
            None,
            Some(StructuredError::new("InvalidToolInput", error.to_string())),
        ),
    }
}

fn tool_content(result: &oc_tools::ToolResult) -> (Vec<ToolContent>, Option<Map<String, Value>>) {
    let mut content = result
        .content
        .as_ref()
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<ToolContent>(item.clone()).ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if content.is_empty() {
        let text = result.output.clone().unwrap_or_default();
        content.push(ToolContent::Text { text });
    }
    (content, metadata_map(result.metadata.as_ref()))
}

fn metadata_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    value
        .and_then(Value::as_object)
        .filter(|map| !map.is_empty())
        .cloned()
}

fn tool_id(state: &AssistantState, index: usize) -> String {
    match state.content.get(index) {
        Some(AssistantPart::Tool(tool)) => tool.id.clone(),
        _ => String::new(),
    }
}

fn ordinal_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "tool-calls" | "tool_use" => FinishReason::ToolCalls,
        "content_filter" | "content-filter" => FinishReason::ContentFilter,
        "error" => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}

fn tokens_from_usage(usage: &Usage) -> Tokens {
    let cache_read = saturating_i64(usage.cache_read_input_tokens);
    let cache_write = saturating_i64(usage.cache_write_input_tokens);
    let non_cached = usage.non_cached_input_tokens.map_or_else(
        || {
            saturating_i64(usage.input_tokens)
                .saturating_sub(cache_read)
                .saturating_sub(cache_write)
        },
        |value| saturating_i64(Some(value)),
    );
    Tokens {
        input: non_cached
            .saturating_add(cache_read)
            .saturating_add(cache_write),
        output: saturating_i64(usage.output_tokens),
        reasoning: saturating_i64(usage.reasoning_tokens),
        cache: oc_types::CacheTokens {
            read: cache_read,
            write: cache_write,
        },
    }
}

fn saturating_i64(value: Option<u64>) -> i64 {
    value.map_or(0, |value| i64::try_from(value).unwrap_or(i64::MAX))
}

fn loaded_skill_ids(messages: &[Value]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for message in messages {
        if message.get("type").and_then(Value::as_str) != Some("skill") {
            continue;
        }
        let id = message
            .get("id")
            .or_else(|| message.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if let Some(id) = id
            && !ids.iter().any(|existing| existing == id)
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

fn assistant_text(state: &AssistantState) -> String {
    state
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) if !text.text.trim().is_empty() => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_tool_lines(stats: &mut TurnStats, message: &AssistantState) {
    for part in &message.content {
        let AssistantPart::Tool(tool) = part else {
            continue;
        };
        let line = match &tool.state {
            ToolState::Completed { content, .. } => {
                let text = tool_text(content);
                format!("- {}: ok{}", tool.name, tool_summary(&text))
            }
            ToolState::Error { error, .. } => {
                format!("- {}: error: {}", tool.name, error.message)
            }
            ToolState::Streaming { .. } | ToolState::Running { .. } => continue,
        };
        stats.tool_lines.push(line);
    }
}

fn tool_text(content: &[ToolContent]) -> String {
    content
        .iter()
        .map(|part| match part {
            ToolContent::Text { text } => text.clone(),
            ToolContent::File { uri, mime, .. } => format!("[file {mime}: {uri}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_summary(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        return String::new();
    }
    let summary: String = first.chars().take(160).collect();
    format!(" ({summary})")
}

fn scribe_digest(stats: &TurnStats) -> String {
    let user: String = stats.user_text.trim().chars().take(1500).collect();
    let assistant: String = stats.assistant_text.trim().chars().take(2000).collect();
    let tools = if stats.tool_lines.is_empty() {
        "(none)".to_owned()
    } else {
        stats.tool_lines.join("\n")
    };
    format!(
        "User intent:\n{user}\n\n\
         Assistant final text:\n{assistant}\n\n\
         Tool outcomes:\n{tools}"
    )
}
