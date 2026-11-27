//! `key_health`: the model-facing view of provider credentials and cooldowns.
//!
//! Rotation itself is silent by design (`docs/27-key-pool.md`): a key that ran
//! out of quota is skipped on the *same* model without an event, so without
//! this tool an agent cannot say which key served a request or why the next one
//! was tried. The tool is read-only and reports no key material: neither
//! [`KeyHealthKey`] nor [`KeyHealthProvider`] has a field for a secret.
//!
//! The tool owns no state. The host binds a [`KeyHealthSource`] in task-local
//! storage for the duration of one tool call ([`with_key_health_source`]); when
//! nothing is bound the tool returns a typed [`ToolError`] instead of inventing
//! a healthy pool.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::{Tool, ToolContext, ToolError, ToolResult};

/// One credential of a provider, already joined with its rotation state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyHealthKey {
    /// Stored credential id (`cred_…`). `None` for a provider's environment
    /// key, which owns no row.
    pub credential_id: Option<String>,
    /// Human label from the credential row.
    pub label: Option<String>,
    /// Whether this is the provider's current primary credential.
    pub active: bool,
    /// Whether the key is out of rotation at `now_ms`.
    pub limited: bool,
    /// Epoch milliseconds at which the cooldown stops applying.
    pub cooldown_until_ms: Option<i64>,
    /// Failure class that started the cooldown (`quota`, `auth`, …).
    pub kind: Option<String>,
    /// Failures recorded for this key in the current cooldown window.
    pub failures: Option<u32>,
    /// `Some("env")` when the key comes from the provider environment.
    pub source: Option<String>,
}

/// Every key of one provider, in rotation order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyHealthProvider {
    /// Provider id as the store and the key pool know it.
    pub provider_id: String,
    /// Active key first, remaining ids ascending, environment key last.
    pub keys: Vec<KeyHealthKey>,
}

/// Verified chat model inventory entry reported when `include_models: true`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyHealthModel {
    pub model_id: String,
    pub reference: String,
    pub name: String,
    pub context_limit: Option<u64>,
    pub output_limit: Option<u64>,
    pub tools: bool,
    pub variants: Vec<String>,
    pub cost_input_usd: Option<f64>,
    pub cost_output_usd: Option<f64>,
    pub favorite: bool,
    pub note: Option<String>,
    pub unavailable_kind: Option<String>,
    pub unavailable_until_ms: Option<i64>,
    pub remaining_percent: Option<f64>,
    pub quota_limited: Option<bool>,
}

/// Combined provider-key, verified-model, and configured agent-default snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyHealthReport {
    pub providers: Vec<KeyHealthProvider>,
    pub provider_models: BTreeMap<String, Vec<KeyHealthModel>>,
    pub agent_models: BTreeMap<String, String>,
}

/// Host-supplied credential state for `key_health`.
///
/// Implemented by the runtime (`oc_runtime::key_health`), which owns the store
/// and the [`oc_llm::KeyPool`](https://docs.rs) equivalent; `oc-tools` stays
/// free of both dependencies.
#[async_trait]
pub trait KeyHealthSource: Send + Sync {
    /// Providers that own at least one key, or exactly the filtered provider.
    ///
    /// `provider` is an exact provider id; `now_ms` is epoch milliseconds and
    /// decides which cooldowns are still live. Implementations must not return
    /// key material.
    async fn providers(
        &self,
        provider: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<KeyHealthProvider>, String>;

    /// Full provider key + optional model/quota report (`include_models: true`).
    async fn report(
        &self,
        provider: Option<&str>,
        include_models: bool,
        now_ms: i64,
    ) -> Result<KeyHealthReport, String> {
        let _ = include_models;
        let providers = self.providers(provider, now_ms).await?;
        Ok(KeyHealthReport {
            providers,
            provider_models: BTreeMap::new(),
            agent_models: BTreeMap::new(),
        })
    }
}

tokio::task_local! {
    static ACTIVE_SOURCE: Arc<dyn KeyHealthSource>;
}

/// Run one tool call with a [`KeyHealthSource`] bound in task-local storage.
///
/// Mirrors [`crate::code_map::with_semantic_oracle`]: the runtime binds the
/// source around `Tool::execute` so [`ToolContext`] keeps its shape.
pub async fn with_key_health_source<F, T>(source: Option<Arc<dyn KeyHealthSource>>, fut: F) -> T
where
    F: Future<Output = T>,
{
    match source {
        Some(source) => ACTIVE_SOURCE.scope(source, fut).await,
        None => fut.await,
    }
}

fn current_source() -> Option<Arc<dyn KeyHealthSource>> {
    ACTIVE_SOURCE.try_with(Arc::clone).ok()
}

/// `key_health`: list provider keys and their cooldowns.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyHealthTool;

#[async_trait]
impl Tool for KeyHealthTool {
    fn name(&self) -> &str {
        "key_health"
    }

    fn description(&self) -> &str {
        "Show the provider credentials this runtime holds and which of them are cooling down after a quota, rate-limit or auth failure. Read-only: returns credential ids, labels and cooldown deadlines, never key material. Pass `include_models: true` (used by `organizer`) to also list selectable tool-capable chat models, cooldowns, quotas, favorites/notes, and configured agent model defaults for connected providers."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type": "string",
                    "description": "Exact provider id to filter by, for example `opencode-go`."
                },
                "include_models": {
                    "type": "boolean",
                    "description": "Include selectable chat models (excluding hidden, deprecated, non-tool, and unconfigured direct entries), limits, costs, unavailability windows, quotas, favorites/notes, and configured agent model defaults for connected providers."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let provider = match input.get("provider") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_owned()),
            Some(Value::String(_)) => {
                return Err(ToolError::new("`provider` must be a non-empty provider id"));
            }
            Some(_) => return Err(ToolError::new("`provider` must be a string")),
        };
        let include_models = match input.get("include_models") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => return Err(ToolError::new("`include_models` must be a boolean")),
        };
        let Some(source) = current_source() else {
            return Err(ToolError::new(
                "key_health is unavailable: no runtime key pool is attached to this session, so no credential state can be reported",
            ));
        };
        let now = now_ms();
        let mut report = source
            .report(provider.as_deref(), include_models, now)
            .await
            .map_err(ToolError::new)?;
        report
            .providers
            .sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
        let summary = summarize(&report.providers, provider.as_deref());
        let providers_json: Vec<Value> = report
            .providers
            .iter()
            .map(|entry| {
                let models = if include_models {
                    report
                        .provider_models
                        .get(&entry.provider_id)
                        .or_else(|| {
                            report
                                .provider_models
                                .get(&entry.provider_id.to_ascii_lowercase())
                        })
                        .map(Vec::as_slice)
                } else {
                    None
                };
                provider_json(entry, models)
            })
            .collect();
        let mut payload = Map::new();
        payload.insert("summary".to_owned(), Value::String(summary));
        payload.insert("providers".to_owned(), Value::Array(providers_json));
        if include_models && !report.agent_models.is_empty() {
            let mut agent_map = Map::new();
            for (agent, reference) in &report.agent_models {
                agent_map.insert(agent.clone(), Value::String(reference.clone()));
            }
            payload.insert("agentModels".to_owned(), Value::Object(agent_map));
        }
        let payload = Value::Object(payload);
        let text = payload.to_string();
        Ok(ToolResult::text(text).with_metadata(payload))
    }
}

fn provider_json(provider: &KeyHealthProvider, models: Option<&[KeyHealthModel]>) -> Value {
    let mut map = Map::new();
    map.insert(
        "providerID".to_owned(),
        Value::String(provider.provider_id.clone()),
    );
    map.insert(
        "keys".to_owned(),
        Value::Array(provider.keys.iter().map(key_json).collect()),
    );
    if let Some(models) = models
        && !models.is_empty()
    {
        map.insert(
            "models".to_owned(),
            Value::Array(models.iter().map(model_json).collect()),
        );
    }
    Value::Object(map)
}

fn model_json(model: &KeyHealthModel) -> Value {
    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(model.model_id.clone()));
    map.insert("ref".to_owned(), Value::String(model.reference.clone()));
    map.insert("name".to_owned(), Value::String(model.name.clone()));
    if let Some(limit) = model.context_limit {
        map.insert("contextLimit".to_owned(), json!(limit));
    }
    if let Some(limit) = model.output_limit {
        map.insert("outputLimit".to_owned(), json!(limit));
    }
    map.insert("tools".to_owned(), Value::Bool(model.tools));
    if !model.variants.is_empty() {
        map.insert("variants".to_owned(), json!(model.variants));
    }
    if let (Some(input), Some(output)) = (model.cost_input_usd, model.cost_output_usd) {
        map.insert(
            "costUsdPer1M".to_owned(),
            json!({ "input": input, "output": output }),
        );
    }
    if model.favorite {
        map.insert("favorite".to_owned(), Value::Bool(true));
    }
    if let Some(note) = model
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        map.insert("note".to_owned(), Value::String(note.to_owned()));
    }
    if let Some(kind) = &model.unavailable_kind {
        let mut unavail = Map::new();
        unavail.insert("kind".to_owned(), Value::String(kind.clone()));
        if let Some(until) = model.unavailable_until_ms {
            unavail.insert("untilMs".to_owned(), json!(until));
        }
        map.insert("unavailable".to_owned(), Value::Object(unavail));
    }
    if model.remaining_percent.is_some() || model.quota_limited.is_some() {
        let mut quota = Map::new();
        if let Some(pct) = model.remaining_percent {
            quota.insert("remainingPercent".to_owned(), json!(pct));
        }
        if let Some(limited) = model.quota_limited {
            quota.insert("limited".to_owned(), json!(limited));
        }
        map.insert("quota".to_owned(), Value::Object(quota));
    }
    Value::Object(map)
}

/// One key, with every optional field omitted rather than `null`.
fn key_json(key: &KeyHealthKey) -> Value {
    let mut map = Map::new();
    if let Some(credential_id) = &key.credential_id {
        map.insert(
            "credentialID".to_owned(),
            Value::String(credential_id.clone()),
        );
    }
    if let Some(label) = &key.label {
        map.insert("label".to_owned(), Value::String(label.clone()));
    }
    map.insert("active".to_owned(), Value::Bool(key.active));
    map.insert("limited".to_owned(), Value::Bool(key.limited));
    if key.limited {
        if let Some(until) = key.cooldown_until_ms {
            map.insert("cooldownUntilMs".to_owned(), json!(until));
        }
        if let Some(kind) = &key.kind {
            map.insert("kind".to_owned(), Value::String(kind.clone()));
        }
        if let Some(failures) = key.failures {
            map.insert("failures".to_owned(), json!(failures));
        }
    }
    if let Some(source) = &key.source {
        map.insert("source".to_owned(), Value::String(source.clone()));
    }
    Value::Object(map)
}

/// One-line paraphrase target for the model.
fn summarize(providers: &[KeyHealthProvider], filter: Option<&str>) -> String {
    if providers.is_empty() {
        return match filter {
            Some(id) => format!("no stored keys or cooldowns for provider `{id}`"),
            None => "no provider keys are configured".to_owned(),
        };
    }
    let mut parts = Vec::with_capacity(providers.len());
    for provider in providers {
        let total = provider.keys.len();
        let cooling: Vec<&KeyHealthKey> = provider.keys.iter().filter(|key| key.limited).collect();
        let usable = total.saturating_sub(cooling.len());
        if cooling.is_empty() {
            parts.push(format!(
                "{total} {} for {}, all healthy",
                key_word(total),
                provider.provider_id
            ));
            continue;
        }
        let mut kinds: Vec<&str> = cooling
            .iter()
            .filter_map(|key| key.kind.as_deref())
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        let kind = if kinds.is_empty() {
            String::new()
        } else {
            format!(" ({})", kinds.join("/"))
        };
        let until = cooling
            .iter()
            .filter_map(|key| key.cooldown_until_ms)
            .min()
            .map_or_else(String::new, |until| format!(" until {}", clock(until)));
        parts.push(format!(
            "{usable} usable {} for {}; {} {} cooling down{kind}{until}",
            key_word(usable),
            provider.provider_id,
            cooling.len(),
            key_word(cooling.len()),
        ));
    }
    parts.join("; ")
}

fn key_word(count: usize) -> &'static str {
    if count == 1 { "key" } else { "keys" }
}

/// `HH:MM UTC`, derived from epoch milliseconds without a date dependency.
fn clock(ms: i64) -> String {
    let minutes = ms.div_euclid(60_000).rem_euclid(1_440);
    format!("{:02}:{:02} UTC", minutes / 60, minutes % 60)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[derive(Debug)]
    struct FakeSource {
        providers: Vec<KeyHealthProvider>,
    }

    #[async_trait]
    impl KeyHealthSource for FakeSource {
        async fn providers(
            &self,
            provider: Option<&str>,
            _now_ms: i64,
        ) -> Result<Vec<KeyHealthProvider>, String> {
            Ok(self
                .providers
                .iter()
                .filter(|entry| match provider {
                    Some(id) => id == entry.provider_id,
                    None => true,
                })
                .cloned()
                .collect())
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "ses_test".to_owned(),
            directory: PathBuf::from("/tmp"),
            cwd: PathBuf::from("/tmp"),
            cancel: None,
        }
    }

    fn healthy(credential_id: &str, label: &str, active: bool) -> KeyHealthKey {
        KeyHealthKey {
            credential_id: Some(credential_id.to_owned()),
            label: Some(label.to_owned()),
            active,
            ..KeyHealthKey::default()
        }
    }

    fn cooling(credential_id: &str) -> KeyHealthKey {
        KeyHealthKey {
            limited: true,
            cooldown_until_ms: Some(1_794_709_019_047),
            kind: Some("quota".to_owned()),
            failures: Some(2),
            ..healthy(credential_id, "Backup", false)
        }
    }

    fn env_key() -> KeyHealthKey {
        KeyHealthKey {
            source: Some("env".to_owned()),
            ..KeyHealthKey::default()
        }
    }

    async fn run(providers: Vec<KeyHealthProvider>, input: Value) -> Value {
        let result = with_key_health_source(
            Some(Arc::new(FakeSource { providers })),
            KeyHealthTool.execute(&ctx(), input),
        )
        .await
        .expect("tool result");
        result.metadata.expect("metadata")
    }

    fn field_names(key: &Value) -> Vec<String> {
        let mut names: Vec<String> = key
            .as_object()
            .expect("key object")
            .keys()
            .cloned()
            .collect();
        names.sort_unstable();
        names
    }

    #[tokio::test]
    async fn a_healthy_key_carries_no_cooldown_fields() {
        let payload = run(
            vec![KeyHealthProvider {
                provider_id: "opencode-go".to_owned(),
                keys: vec![healthy("cred_a", "OpenCode Go", true), env_key()],
            }],
            json!({}),
        )
        .await;
        let keys = payload["providers"][0]["keys"].as_array().expect("keys");
        assert_eq!(
            field_names(&keys[0]),
            vec!["active", "credentialID", "label", "limited"]
        );
        assert_eq!(keys[0]["limited"], json!(false));
        assert_eq!(
            field_names(&keys[1]),
            vec!["active", "limited", "source"],
            "the environment key has no credentialID"
        );
        assert_eq!(keys[1]["source"], json!("env"));
        let summary = payload["summary"].as_str().expect("summary");
        assert!(summary.contains("all healthy"), "{summary}");
        assert!(summary.contains("2 keys for opencode-go"), "{summary}");
    }

    #[tokio::test]
    async fn a_cooling_key_reports_its_deadline_and_class() {
        let payload = run(
            vec![KeyHealthProvider {
                provider_id: "opencode-go".to_owned(),
                keys: vec![healthy("cred_a", "OpenCode Go", true), cooling("cred_b")],
            }],
            json!({}),
        )
        .await;
        let keys = payload["providers"][0]["keys"].as_array().expect("keys");
        assert_eq!(
            field_names(&keys[1]),
            vec![
                "active",
                "cooldownUntilMs",
                "credentialID",
                "failures",
                "kind",
                "label",
                "limited"
            ]
        );
        assert_eq!(keys[1]["cooldownUntilMs"], json!(1_794_709_019_047_i64));
        assert_eq!(keys[1]["kind"], json!("quota"));
        assert_eq!(keys[1]["failures"], json!(2));
        let summary = payload["summary"].as_str().expect("summary");
        assert!(summary.contains("1 usable key"), "{summary}");
        assert!(summary.contains("cooling down (quota) until"), "{summary}");
    }

    #[tokio::test]
    async fn the_provider_filter_selects_exactly_one_provider() {
        let payload = run(
            vec![
                KeyHealthProvider {
                    provider_id: "opencode-go".to_owned(),
                    keys: vec![healthy("cred_a", "OpenCode Go", true)],
                },
                KeyHealthProvider {
                    provider_id: "anthropic".to_owned(),
                    keys: vec![healthy("cred_x", "Anthropic", true)],
                },
            ],
            json!({"provider": "anthropic"}),
        )
        .await;
        let providers = payload["providers"].as_array().expect("providers");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["providerID"], json!("anthropic"));

        let filtered = run(
            vec![KeyHealthProvider {
                provider_id: "opencode-go".to_owned(),
                keys: vec![healthy("cred_a", "OpenCode Go", true)],
            }],
            json!({"provider": "opencode-go"}),
        )
        .await;
        assert_eq!(
            filtered["providers"].as_array().expect("providers").len(),
            1
        );
    }

    #[tokio::test]
    async fn an_unknown_provider_reports_an_empty_layout_not_an_error() {
        let payload = run(Vec::new(), json!({"provider": "nope"})).await;
        assert_eq!(payload["providers"], json!([]));
        let summary = payload["summary"].as_str().expect("summary");
        assert!(summary.contains("no stored keys"), "{summary}");
    }

    #[tokio::test]
    async fn without_a_source_the_tool_reports_a_typed_error() {
        let error = KeyHealthTool
            .execute(&ctx(), json!({}))
            .await
            .expect_err("no source is bound");
        assert!(error.to_string().contains("key pool"), "{error}");
    }

    #[tokio::test]
    async fn the_result_exposes_only_the_documented_fields() {
        // Structural no-leak proof: the serialized key object is an allowlist,
        // so a secret has no field that could carry it.
        let payload = run(
            vec![KeyHealthProvider {
                provider_id: "opencode-go".to_owned(),
                keys: vec![cooling("cred_b"), env_key()],
            }],
            json!({}),
        )
        .await;
        let text = payload.to_string();
        for forbidden in ["key\"", "secret", "apiKey", "access", "token"] {
            assert!(
                !text.contains(forbidden),
                "`{forbidden}` must not appear in the result: {text}"
            );
        }
    }

    #[tokio::test]
    async fn include_models_surfaces_models_and_agent_defaults_only_when_requested() {
        #[derive(Debug)]
        struct ModelSource;

        #[async_trait]
        impl KeyHealthSource for ModelSource {
            async fn providers(
                &self,
                _provider: Option<&str>,
                _now_ms: i64,
            ) -> Result<Vec<KeyHealthProvider>, String> {
                Ok(vec![KeyHealthProvider {
                    provider_id: "opencode-go".to_owned(),
                    keys: vec![healthy("cred_a", "OpenCode Go", true)],
                }])
            }

            async fn report(
                &self,
                provider: Option<&str>,
                include_models: bool,
                now_ms: i64,
            ) -> Result<KeyHealthReport, String> {
                let providers = self.providers(provider, now_ms).await?;
                if !include_models {
                    return Ok(KeyHealthReport {
                        providers,
                        ..KeyHealthReport::default()
                    });
                }
                let mut provider_models = BTreeMap::new();
                provider_models.insert(
                    "opencode-go".to_owned(),
                    vec![
                        KeyHealthModel {
                            model_id: "fast-1".to_owned(),
                            reference: "opencode-go/fast-1".to_owned(),
                            name: "Fast 1".to_owned(),
                            context_limit: Some(128_000),
                            output_limit: Some(16_384),
                            tools: true,
                            favorite: true,
                            note: Some("fast coding default".to_owned()),
                            ..KeyHealthModel::default()
                        },
                        KeyHealthModel {
                            model_id: "fast-2".to_owned(),
                            reference: "opencode-go/fast-2".to_owned(),
                            name: "Fast 2".to_owned(),
                            context_limit: Some(128_000),
                            output_limit: Some(16_384),
                            tools: true,
                            ..KeyHealthModel::default()
                        },
                    ],
                );
                let mut agent_models = BTreeMap::new();
                agent_models.insert("organizer".to_owned(), "opencode-go/fast-1".to_owned());
                Ok(KeyHealthReport {
                    providers,
                    provider_models,
                    agent_models,
                })
            }
        }

        let without = with_key_health_source(
            Some(Arc::new(ModelSource)),
            KeyHealthTool.execute(&ctx(), json!({})),
        )
        .await
        .expect("default")
        .metadata
        .expect("metadata");
        assert!(without["providers"][0].get("models").is_none());
        assert!(without.get("agentModels").is_none());

        let with_res = with_key_health_source(
            Some(Arc::new(ModelSource)),
            KeyHealthTool.execute(&ctx(), json!({"include_models": true})),
        )
        .await
        .expect("with models");
        let compact = with_res.output.expect("compact text");
        assert!(
            !compact.contains('\n'),
            "key_health output uses compact single-line JSON: {compact}"
        );
        let with = with_res.metadata.expect("metadata");
        assert_eq!(
            with["providers"][0]["models"][0]["ref"],
            json!("opencode-go/fast-1")
        );
        assert_eq!(with["providers"][0]["models"][0]["favorite"], json!(true));
        assert_eq!(
            with["providers"][0]["models"][0]["note"],
            json!("fast coding default")
        );
        assert!(with["providers"][0]["models"][1].get("favorite").is_none());
        assert!(with["providers"][0]["models"][1].get("note").is_none());
        assert_eq!(
            with["agentModels"]["organizer"],
            json!("opencode-go/fast-1")
        );
    }
}
