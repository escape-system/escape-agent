use std::time::Duration;

use oc_store::{NewSession, NewSessionTree};
use oc_subagents::{BudgetError, Policy, SpawnError};
use oc_tools::{ToolContext, ToolError};
use oc_types::SessionId;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

use super::super::settings::{
    SubagentSettings, available_agents, available_models, model_defaults, model_key, model_value,
    requested_role,
};
use super::super::tree::{
    budget_error, load_info, load_node, optional_str, runtime_error, spawn_error, store_error,
};
use super::super::wait::enqueue_prompt;
use super::{PartitionMeta, SpawnContext, SpawnedPartitionChild};

pub(super) fn parse_optional_usize(input: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    let Some(val) = input.get(key) else {
        return Ok(None);
    };
    if val.is_null() {
        return Ok(None);
    }
    let Some(n) = val.as_u64() else {
        return Err(ToolError::new(format!(
            "{key} must be a non-negative integer"
        )));
    };
    usize::try_from(n)
        .map(Some)
        .map_err(|_| ToolError::new(format!("{key} is too large")))
}

pub(super) async fn resolve_manual_partition(
    ctx: &ToolContext,
    prompt: &str,
    partition_index: Option<usize>,
    partition_count: Option<usize>,
) -> Result<Option<(String, PartitionMeta)>, ToolError> {
    match (partition_index, partition_count) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(ToolError::new(
            "partition_index and partition_count must both be provided together",
        )),
        (Some(idx), Some(count)) => {
            if !(2..=16).contains(&count) {
                return Err(ToolError::new("partition_count must be between 2 and 16"));
            }
            if idx >= count {
                return Err(ToolError::new(format!(
                    "partition_index ({idx}) must be less than partition_count ({count})"
                )));
            }
            let partitioned =
                oc_tools::code_map::partition_task(ctx, &ctx.directory, prompt, count, 2000)
                    .await?;
            let section = partitioned.prompt_section_for(idx).unwrap_or_default();
            let files = partitioned
                .slices
                .iter()
                .find(|s| s.index == idx)
                .map(|s| s.files.clone())
                .unwrap_or_default();
            Ok(Some((
                section,
                PartitionMeta {
                    index: idx,
                    count: partitioned.slices.len().max(1),
                    files,
                },
            )))
        }
    }
}

// Long because the spawn decision has to report every refusal (limits, model,
// quota) before any session row exists; splitting it would hide that ordering.
#[allow(clippy::too_many_lines)]
pub(super) async fn prepare_spawn_context(
    runtime: &Runtime,
    caller: &str,
    agent: &str,
    input: &Value,
    settings: &SubagentSettings,
) -> Result<SpawnContext, ToolError> {
    let mut parent = load_node(runtime, caller).await?;
    let mut limits = settings.limits.clone();
    if let Some(over) = settings.overrides.get(&parent.agent) {
        if over.allow_spawn == Some(false) {
            return Err(spawn_error(&SpawnError::SpawnNotAllowed {
                id: caller.to_owned(),
            }));
        }
        if let Some(max_children) = over.max_children {
            limits.max_children = max_children;
        }
    }
    let active = runtime
        .inner
        .config
        .store
        .active_child_count(caller)
        .await
        .unwrap_or(parent.child_count);
    if active < parent.child_count {
        parent.child_count = active;
    }
    let available = available_agents(&runtime.inner.config.config);
    let policy = Policy::new(limits.clone());
    let requested_depth = input
        .get("spawn")
        .and_then(|spawn| spawn.get("max_depth"))
        .and_then(Value::as_u64)
        .map(|depth| u32::try_from(depth).unwrap_or(u32::MAX));
    let mut plan = policy
        .spawn(&parent, agent, requested_depth, &available)
        .map_err(|error| spawn_error(&error))?;

    if let Some(over) = settings.overrides.get(agent)
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

    let parent_model = parent
        .model
        .clone()
        .or_else(|| runtime.inner.config.config.model())
        .unwrap_or_default();
    let defaults = model_defaults(&runtime.inner.config.config, settings);
    let models = &settings.models;
    if let Some(error) = &models.error {
        // A half-read policy must never pick a child model: refuse before any
        // session row is written, naming the offending field.
        return Err(ToolError::new(format!("ModelsConfigError: {error}")));
    }
    let available = available_models(
        &runtime.inner.config.catalog.current(),
        &runtime.inner.config.client,
    );
    let chain = models.chain_for(agent);
    let call_model = optional_str(input, "model");

    // The candidate list in resolution order, with provenance. Built here so the
    // quota scan, the health filter and the typed `rejectedModels` report can all
    // name a model's origin; `resolve_model_with` rebuilds the same order.
    let mut ordered: Vec<(String, oc_subagents::ModelSource)> = Vec::new();
    let push_ordered =
        |list: &mut Vec<(String, oc_subagents::ModelSource)>, reference: &str, source| {
            if reference.trim().is_empty() {
                return;
            }
            if list.iter().any(|(seen, _)| seen == reference) {
                return;
            }
            list.push((reference.to_owned(), source));
        };
    if let Some(reference) = call_model.as_deref() {
        push_ordered(&mut ordered, reference, oc_subagents::ModelSource::Call);
    }
    for reference in chain {
        push_ordered(&mut ordered, reference, oc_subagents::ModelSource::Chain);
    }
    if let Some(reference) = defaults.agent_models.get(agent) {
        push_ordered(&mut ordered, reference, oc_subagents::ModelSource::Agent);
    }
    if let Some(reference) = defaults.policy_models.get(agent) {
        push_ordered(&mut ordered, reference, oc_subagents::ModelSource::Agent);
    }
    if let Some(reference) = defaults.default_model.as_deref() {
        push_ordered(&mut ordered, reference, oc_subagents::ModelSource::Default);
    }
    push_ordered(
        &mut ordered,
        &parent_model,
        oc_subagents::ModelSource::Parent,
    );
    // Variant-insensitive like the quota scan: `candidates` dedups on
    // `provider/id` while `ordered` dedups on the exact reference, so a denied
    // variant must still resolve to the source it was proposed with.
    let source_of = |reference: &str| {
        let key = crate::subagent::settings::model_key(reference);
        ordered
            .iter()
            .find(|(seen, _)| crate::subagent::settings::model_key(seen) == key)
            .map_or(oc_subagents::ModelSource::Chain, |(_, source)| *source)
    };

    // Variant-insensitive de-duplication for the quota scan.
    let push_candidate = |list: &mut Vec<String>, reference: &str| {
        if reference.trim().is_empty() {
            return;
        }
        let key = model_key(reference);
        if list.iter().any(|seen| model_key(seen) == key) {
            return;
        }
        list.push(reference.to_owned());
    };

    // Every candidate the resolver could pick, in resolution order. The list is
    // rebuilt inside `resolve_model_with`; the copy here exists so quota can be
    // evaluated for ALL candidates — a `models.quota` without a `models.chain`
    // must still stop an exhausted parent/default model.
    let mut candidates: Vec<String> = Vec::new();
    if let Some(reference) = call_model.as_deref() {
        push_candidate(&mut candidates, reference);
    }
    if let Some(reference) = defaults.agent_models.get(agent) {
        push_candidate(&mut candidates, reference);
    }
    if let Some(reference) = defaults.policy_models.get(agent) {
        push_candidate(&mut candidates, reference);
    }
    if let Some(reference) = defaults.default_model.as_deref() {
        push_candidate(&mut candidates, reference);
    }
    push_candidate(&mut candidates, &parent_model);
    for reference in chain {
        push_candidate(&mut candidates, reference);
    }

    let mut denied: Vec<String> = Vec::new();
    let mut quota_denials: Vec<(String, String)> = Vec::new();
    if let Some(quota) = &models.quota {
        // The usage rollup comes from the store (one GROUP BY), never a session
        // full scan.
        let window_ms = i64::from(quota.window_hours) * 3_600_000;
        let usage = runtime
            .inner
            .config
            .store
            .model_usage(oc_types::now_millis().saturating_sub(window_ms))
            .await
            .map_err(|error| store_error(&error))?;
        let fraction = quota.usable_fraction();
        // `(100 - reserve_percent)` as an integer so request counts never go
        // through a lossy float.
        let paid_percent = 100_u128.saturating_sub(u128::from(quota.reserve_percent));
        for reference in &candidates {
            let Some(budget) = quota.budget_for(reference) else {
                continue;
            };
            let key = crate::subagent::settings::model_key(reference);
            let normalize = |value: &str| value.to_ascii_lowercase().replace('.', "-");
            let row = usage.iter().find(|row| {
                normalize(&key) == normalize(&format!("{}/{}", row.provider_id, row.model_id))
            });
            let cost = row.map_or(0.0, |row| row.cost);
            let requests = u64::try_from(row.map_or(0, |row| row.requests)).unwrap_or(0);
            let cost_cap = budget.max_cost_usd.map(|max| max * fraction);
            let over_cost = cost_cap.is_some_and(|cap| cost >= cap);
            let over_requests = budget
                .max_requests
                .is_some_and(|max| u128::from(requests) * 100 >= u128::from(max) * paid_percent);
            if over_cost || over_requests {
                let reason = if over_cost {
                    format!("quota_cost {cost:.2}/{:.2}", cost_cap.unwrap_or_default())
                } else {
                    format!(
                        "quota_requests {requests}/{}",
                        budget.max_requests.unwrap_or_default()
                    )
                };
                denied.push(reference.clone());
                quota_denials.push((reference.clone(), reason));
            }
        }
        if !quota_denials.is_empty() {
            tracing::debug!(
                ?quota_denials,
                agent,
                "models.quota denied spawn candidates"
            );
        }
    }

    // Models the turn fallback just watched fail in a model-level class are
    // removed from this spawn's candidate list: paying the same failed round
    // trip would only re-teach the same lesson. The reasons for such skips are
    // typed later (Denied, with the candidate's own source).
    let now = oc_types::now_millis();
    let mut unhealthy: Vec<String> = Vec::new();
    for (reference, _) in &ordered {
        if denied.contains(reference) {
            continue;
        }
        if !runtime.model_health().is_available(reference, now) {
            unhealthy.push(reference.clone());
            denied.push(reference.clone());
        }
    }

    let filter = oc_subagents::model::CandidateFilter::new(chain, &denied);
    let choice = oc_subagents::model::resolve_model_with(
        call_model.as_deref(),
        Some(agent),
        defaults,
        &parent_model,
        &available,
        Some(&filter),
    )
    .map_err(|error| {
        let denials = quota_denials
            .iter()
            .map(|(model, reason)| format!("{model} ({reason})"))
            .collect::<Vec<_>>()
            .join(", ");
        if denials.is_empty() {
            ToolError::new(format!("ModelUnavailableError: {error}"))
        } else {
            ToolError::new(format!(
                "ModelUnavailableError: {error}; quota-denied: {denials}"
            ))
        }
    })?;

    // One typed skip list for both sources of denial: the quota scan keeps its
    // human-readable reason in the error text, while `rejectedModels` reports
    // `{reference, source, reason}`. Resolver skips (malformed / absent from the
    // catalog / denied) keep their own typed reason; a reference denied by quota
    // or health is `Denied`, carrying the source it was proposed with.
    let mut rejected_models: Vec<oc_subagents::ModelSkip> = Vec::new();
    for (reference, reason) in &quota_denials {
        rejected_models.push(oc_subagents::ModelSkip::new(
            reference.clone(),
            source_of(reference),
            oc_subagents::SkipReason::Denied,
        ));
        tracing::debug!(reference, reason, "models.quota denial");
    }
    for (reference, source) in &ordered {
        if unhealthy.iter().any(|seen| seen == reference) {
            rejected_models.push(oc_subagents::ModelSkip::new(
                reference.clone(),
                *source,
                oc_subagents::SkipReason::Denied,
            ));
        }
    }
    for skip in &choice.skipped {
        if rejected_models
            .iter()
            .any(|seen| seen.reference == skip.reference)
        {
            continue;
        }
        rejected_models.push(skip.clone());
    }
    let model_value = model_value(
        &choice.reference,
        &runtime.inner.config.catalog.current(),
        &runtime.inner.config.client,
    )?;
    let created_model: Option<oc_types::ModelRef> =
        serde_json::from_value(model_value.clone()).ok();
    // Key cooldowns for the chosen model's provider, so the caller can see that
    // the agents it spawns are working around an exhausted or revoked backup key
    // (`docs/27-key-pool.md`). Only the chosen provider is reported: a cooldown
    // on an unrelated provider says nothing about this child. Cooldowns are
    // process-local memory, so this is a snapshot, not a durable fact.
    let key_health: Vec<serde_json::Value> = match &created_model {
        Some(model) => {
            let provider = model.provider_id.trim().to_ascii_lowercase();
            runtime
                .key_pool()
                .entries(oc_types::now_millis())
                .into_iter()
                .filter(|entry| entry.provider == provider)
                .map(|entry| {
                    let mut value = json!({
                        "untilMs": entry.until_ms,
                        "kind": entry.kind.as_str(),
                        "failures": entry.failures,
                    });
                    // The environment key has no credential row, so the field is
                    // omitted rather than reported as `null`.
                    if let Some(credential_id) = entry.credential_id {
                        value["credentialID"] = json!(credential_id);
                    }
                    value
                })
                .collect()
        }
        None => Vec::new(),
    };
    // Quota snapshot for the model that actually won. Computed whenever a quota
    // policy exists (it now covers every candidate, chain or not), so the
    // no-quota path still performs exactly the same store reads as before.
    let quota_snapshot = match &settings.models.quota {
        Some(quota) => {
            let window_ms = i64::from(quota.window_hours) * 3_600_000;
            let usage = runtime
                .inner
                .config
                .store
                .model_usage(oc_types::now_millis().saturating_sub(window_ms))
                .await
                .map_err(|error| store_error(&error))?;
            let key = crate::subagent::settings::model_key(&choice.reference);
            let provider_id = key
                .split_once('/')
                .map_or_else(String::new, |(provider, _)| provider.to_owned());
            let normalize = |value: &str| value.to_ascii_lowercase().replace('.', "-");
            let row = usage.iter().find(|row| {
                normalize(&key) == normalize(&format!("{}/{}", row.provider_id, row.model_id))
            });
            let spend = row.map(|row| row.cost);
            let remaining_pct = quota.budget_for(&choice.reference).and_then(|budget| {
                let max = budget.max_cost_usd?;
                if max <= 0.0 {
                    return Some(0.0);
                }
                Some((((max - spend.unwrap_or(0.0)) / max) * 100.0).clamp(0.0, 100.0))
            });
            Some(super::QuotaSnapshot {
                provider_id,
                remaining_pct,
                limited: remaining_pct.is_some_and(|pct| pct <= 0.0),
                spend_usd: spend,
                window_hours: quota.window_hours,
            })
        }
        _ => None,
    };
    let parent_info = load_info(runtime, caller).await?;
    let role = requested_role(input, settings, agent);
    let lane = optional_str(input, "lane");
    let group = optional_str(input, "group").and_then(|g| {
        let trimmed = g.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    });

    Ok(SpawnContext {
        runtime: runtime.clone(),
        caller: caller.to_owned(),
        agent: agent.to_owned(),
        settings: settings.clone(),
        limits,
        active_children: parent.child_count,
        plan,
        parent_root_id: parent.root_id,
        parent_info,
        model_value,
        created_model,
        model_source: choice.source,
        quota_snapshot,
        key_health,
        rejected_models,
        role,
        lane,
        group,
    })
}

pub(super) async fn reserve_spawn_slots(
    runtime: &Runtime,
    caller: &str,
    limits: &oc_subagents::Limits,
    active_children: u32,
    count: u32,
    cancel: Option<&CancellationToken>,
) -> Result<(), ToolError> {
    loop {
        let result = {
            let mut state = runtime.inner.subagents.lock().expect("subagent state");
            state
                .spawn_budget(caller, limits, active_children)
                .try_spawn_batch(count, oc_types::now_millis())
        };
        match result {
            Ok(()) => return Ok(()),
            Err(
                ref error @ BudgetError::RateLimited {
                    retry_after_ms,
                    max_per_minute,
                    ..
                },
            ) if max_per_minute >= count && retry_after_ms > 0 && retry_after_ms <= 25_000 => {
                let wait_ms = u64::try_from(retry_after_ms.saturating_add(25)).unwrap_or(25);
                let wait = Duration::from_millis(wait_ms);
                if let Some(cancel) = cancel {
                    tokio::select! {
                        () = cancel.cancelled() => return Err(budget_error(error)),
                        () = tokio::time::sleep(wait) => {}
                    }
                } else {
                    tokio::time::sleep(wait).await;
                }
            }
            Err(error) => return Err(budget_error(&error)),
        }
    }
}

pub(super) async fn create_and_notify_child(
    ctx_spawn: &SpawnContext,
    description: &str,
    prompt: &str,
    partition: Option<&PartitionMeta>,
) -> Result<SpawnedPartitionChild, ToolError> {
    let runtime = &ctx_spawn.runtime;
    let child = SessionId::new().into_inner();
    let mut meta = serde_json::Map::new();
    if let Some(lane) = &ctx_spawn.lane {
        meta.insert("lane".to_owned(), Value::String(lane.clone()));
    }
    if let Some(group) = &ctx_spawn.group {
        meta.insert("group".to_owned(), Value::String(group.clone()));
    }
    if let Some(part) = partition {
        meta.insert("partition_index".to_owned(), json!(part.index));
        meta.insert("partition_count".to_owned(), json!(part.count));
        meta.insert("partition_files".to_owned(), json!(part.files));
    }
    let metadata = if meta.is_empty() {
        None
    } else {
        Some(Value::Object(meta))
    };
    let created_metadata = metadata.as_ref().and_then(Value::as_object).cloned();

    runtime
        .inner
        .config
        .store
        .create_session_with_tree(
            NewSession {
                id: child.clone(),
                parent_id: Some(ctx_spawn.caller.clone()),
                project_id: ctx_spawn.parent_info.project_id.to_string(),
                directory: ctx_spawn.parent_info.location.directory.clone(),
                title: Some(description.to_owned()),
                agent: Some(ctx_spawn.agent.clone()),
                model: Some(ctx_spawn.model_value.clone()),
                metadata,
                permissions: None,
                time_created: oc_types::now_millis(),
            },
            NewSessionTree {
                root_id: ctx_spawn.parent_root_id.clone(),
                depth: ctx_spawn.plan.depth,
                role: ctx_spawn.role.as_str().to_owned(),
                spawn_allowed: ctx_spawn.plan.spawn_allowed,
            },
        )
        .await
        .map_err(|error| store_error(&error))?;

    runtime
        .emit(
            &child,
            &ctx_spawn.parent_info.location.directory,
            oc_types::payload::SessionCreated {
                session_id: SessionId::from_wire(child.clone()),
                project_id: ctx_spawn.parent_info.project_id.clone(),
                location: ctx_spawn.parent_info.location.clone(),
                subpath: ctx_spawn.parent_info.subpath.clone(),
                parent_id: Some(SessionId::from_wire(ctx_spawn.caller.clone())),
                slug: format!("subagent-{}", &child[child.len().saturating_sub(6)..]),
                title: Some(description.to_owned()),
                agent: Some(ctx_spawn.agent.clone()),
                model: ctx_spawn.created_model.clone(),
                metadata: created_metadata,
                permissions: None,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        )
        .await
        .map_err(|error| runtime_error(&error))?;

    let prompt_id = enqueue_prompt(runtime, &child, prompt, &ctx_spawn.caller).await?;
    runtime
        .notify(&child)
        .await
        .map_err(|error| runtime_error(&error))?;

    Ok(SpawnedPartitionChild {
        session_id: child,
        prompt_id,
        partition_index: partition.map_or(0, |p| p.index),
        partition_count: partition.map_or(1, |p| p.count),
        files: partition.map(|p| p.files.clone()).unwrap_or_default(),
    })
}
