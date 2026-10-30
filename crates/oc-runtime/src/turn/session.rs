use std::sync::Arc;

use oc_config::Effect as ConfigEffect;
use oc_config::PermissionRule;
use oc_llm::ToolDef;
use oc_memory::{MemoryTools, NewMemoryInput};
use oc_tools::ToolRegistry;
use oc_types::{Effect as WireEffect, ModelRef, SessionInfo};
use serde_json::Value;

use crate::expectation::{self};
use crate::instructions::InstructionSet;
use crate::prompt::{self};
use crate::runtime::{ActorHandle, Runtime, RuntimeError};

use super::permissions::tool_action;
use super::stats::TurnStats;
use super::stats::scribe_digest;
use super::types::{ResolvedTurn, SessionInstructionsUpdated};

const DEFAULT_STEPS: u64 = 50;

impl Runtime {
    /// Stored credential for `provider`, preferring the integration's active
    /// row. Returns the tagged value's `key`, or an OAuth token as fallback.
    /// Store failures are logged; the client still falls back to the provider's
    /// environment variables.
    pub(super) async fn credential_key(&self, provider: &str) -> Option<String> {
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

    pub(super) fn resolve_turn(
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
    pub(super) async fn refresh_instructions(
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
    pub(super) async fn touch_memory(&self, ids: &[String]) {
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
    pub(super) async fn spawn_scribe(&self, session: &str, stats: &TurnStats) {
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

    pub(super) fn effect(
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
}

pub(super) fn session_rules(info: &SessionInfo) -> Vec<PermissionRule> {
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
