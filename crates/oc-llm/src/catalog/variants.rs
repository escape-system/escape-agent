//! Reasoning variant synthesis per protocol.

use std::sync::Arc;

use serde_json::{Map, Value};

use super::cost::unsigned;
use super::source::SourceReasoning;
use super::types::{ModelInfo, Protocol, Variant};
use super::util::strings;

// ---------------------------------------------------------------------------
// Variants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(super) struct ReasoningSupport {
    pub(super) supported: bool,
    pub(super) toggle: bool,
    pub(super) effort: Option<Vec<String>>,
    pub(super) budget: Option<(Option<i64>, Option<i64>)>,
}

impl ReasoningSupport {
    pub(super) fn from_source(source: &SourceReasoning) -> Self {
        if !source.supported {
            return Self::default();
        }
        Self {
            supported: true,
            toggle: source.toggle,
            effort: source.effort.as_ref().map(|values| {
                values
                    .iter()
                    .filter(|value| value.as_str() != "default")
                    .cloned()
                    .collect()
            }),
            budget: source
                .budget
                .as_ref()
                .map(|budget| (budget.min, budget.max)),
        }
    }
}

pub(super) fn reasoning_support_from_config(value: &Value) -> ReasoningSupport {
    let Some(object) = value.as_object() else {
        return ReasoningSupport::default();
    };
    if object.get("supported").and_then(Value::as_bool) != Some(true) {
        return ReasoningSupport::default();
    }
    ReasoningSupport {
        supported: true,
        toggle: object
            .get("toggle")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        effort: object
            .get("effort")
            .and_then(Value::as_array)
            .map(|values| strings(values))
            .map(|values| {
                values
                    .into_iter()
                    .filter(|value| value != "default")
                    .collect()
            }),
        budget: object
            .get("budget")
            .and_then(Value::as_object)
            .map(|budget| {
                (
                    budget.get("min").and_then(Value::as_i64),
                    budget.get("max").and_then(Value::as_i64),
                )
            }),
    }
}

#[derive(Clone, Copy)]
enum SupportKind<'a> {
    Effort(&'a [String]),
    Toggle,
    Budget((Option<i64>, Option<i64>)),
}

#[allow(
    clippy::too_many_lines,
    reason = "package-based dispatch mirrors upstream variant synthesis in one table"
)]
pub(super) fn variants_for(
    model: &ModelInfo,
    package: &str,
    protocol: Protocol,
    support: &ReasoningSupport,
) -> Vec<Variant> {
    let has_controls = support.toggle || support.effort.is_some() || support.budget.is_some();
    if !support.supported && !has_controls {
        return Vec::new();
    }
    if protocol == Protocol::OpenAiChat
        && model.provider_id == "opencode-go"
        && model.id == "hungerg-free"
    {
        return vec![with_settings(
            "max",
            Map::from_iter([(
                "reasoningEffort".to_string(),
                Value::String("max".to_string()),
            )]),
        )];
    }
    let package_lower = package.to_ascii_lowercase();
    let toggle = if support.toggle {
        resolve_for_package(model, &package_lower, protocol, SupportKind::Toggle)
    } else {
        Vec::new()
    };
    let main = if let Some(values) = support.effort.as_deref() {
        resolve_for_package(model, &package_lower, protocol, SupportKind::Effort(values))
    } else if let Some(budget) = support.budget {
        resolve_for_package(model, &package_lower, protocol, SupportKind::Budget(budget))
    } else {
        toggle.clone()
    };

    let mut combined: Vec<Variant> = toggle
        .into_iter()
        .filter(|variant| variant.id == "none")
        .chain(main)
        .collect();
    let mut seen = Vec::with_capacity(combined.len());
    combined.retain(|variant| {
        if seen.contains(&variant.id) {
            false
        } else {
            seen.push(variant.id.clone());
            true
        }
    });
    combined
}

fn resolve_for_package(
    model: &ModelInfo,
    package: &str,
    protocol: Protocol,
    support: SupportKind<'_>,
) -> Vec<Variant> {
    match package {
        "@opencode/ai/providers/deepseek" => return deepseek_variants(support),
        "@opencode/ai/providers/zai"
        | "@opencode/ai/providers/zai/chat"
        | "@opencode/ai/providers/zai-coding-plan"
        | "@opencode/ai/providers/zai-coding-plan/chat" => {
            return zai_chat_variants(model, package, support);
        }
        "@opencode/ai/providers/zai-coding-plan/messages" => {
            return zai_messages_variants(model, support);
        }
        "@opencode/ai/providers/moonshot" | "@opencode/ai/providers/moonshot/chat" => {
            return moonshot_chat_variants(model, support);
        }
        "@opencode/ai/providers/moonshot/messages" => {
            return moonshot_messages_variants(model, support);
        }
        "@opencode/ai/providers/alibaba" | "@opencode/ai/providers/alibaba/chat" => {
            return alibaba_chat_variants(model, support);
        }
        "@opencode/ai/providers/alibaba/messages" => {
            return alibaba_messages_variants(model, support);
        }
        "@opencode/ai/providers/minimax" | "@opencode/ai/providers/minimax/messages"
            if protocol == Protocol::AnthropicMessages =>
        {
            return minimax_messages_variants(model, support);
        }
        "@opencode/ai/providers/openrouter" => return openrouter_variants(model, support),
        "@opencode/ai/providers/venice" | "aisdk:venice-ai-sdk-provider" => {
            return venice_variants(support);
        }
        "@opencode/ai/providers/deepinfra" => return deepinfra_variants(model, support),
        "@opencode/ai/providers/cloudflare-workers-ai" => return workers_ai_variants(support),
        "@opencode/ai/providers/baseten" => return baseten_variants(model, support),
        "@opencode/ai/providers/google-vertex/chat" => return vertex_chat_variants(model, support),
        "@opencode/ai/providers/openai-compatible" if model.provider_id == "nvidia" => {
            return nvidia_variants(model, support);
        }
        _ => {}
    }

    match protocol {
        Protocol::OpenAiChat => openai_variants(model, support),
        Protocol::AnthropicMessages => anthropic_variants(model, support),
        Protocol::Gemini | Protocol::GeminiInteractions => gemini_variants(model, support),
        Protocol::Unsupported => Vec::new(),
    }
}

fn settings_variant(id: &str, settings: &Value) -> Variant {
    Variant {
        id: id.to_string(),
        settings: settings.as_object().map(|map| Arc::new(map.clone())),
        headers: None,
        body: None,
    }
}

fn body_variant(id: &str, body: &Value) -> Variant {
    Variant {
        id: id.to_string(),
        settings: None,
        headers: None,
        body: body.as_object().map(|map| Box::new(map.clone())),
    }
}

fn with_settings(id: &str, inner: Map<String, Value>) -> Variant {
    settings_variant(id, &Value::Object(inner))
}

fn with_settings_and_body(id: &str, settings: &Value, body: &Value) -> Variant {
    Variant {
        id: id.to_string(),
        settings: settings.as_object().map(|map| Arc::new(map.clone())),
        headers: None,
        body: body.as_object().map(|map| Box::new(map.clone())),
    }
}

fn effort_values(values: &[String], defaults: &[&str]) -> Vec<String> {
    if values.is_empty() {
        defaults.iter().map(|value| (*value).to_string()).collect()
    } else {
        values.to_vec()
    }
}

fn openai_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    // Both Go feeds mark HungerG reasoning-capable but omit its effort list.
    // Pin its maximum in the shared synthesizer so feed parsing and later
    // config overlays cannot drop the selectable `#max` variant.
    if model.provider_id == "opencode-go" && model.id == "hungerg-free" {
        return vec![with_settings(
            "max",
            Map::from_iter([(
                "reasoningEffort".to_string(),
                Value::String("max".to_string()),
            )]),
        )];
    }
    let SupportKind::Effort(values) = support else {
        return Vec::new();
    };
    effort_values(values, &["low", "medium", "high"])
        .into_iter()
        .map(|effort| {
            with_settings(
                &effort,
                Map::from_iter([("reasoningEffort".to_string(), Value::String(effort.clone()))]),
            )
        })
        .collect()
}

fn deepseek_variants(support: SupportKind<'_>) -> Vec<Variant> {
    match support {
        SupportKind::Effort(values) => effort_values(values, &["low", "high", "max"])
            .into_iter()
            .map(|effort| {
                if effort == "none" {
                    body_variant(
                        &effort,
                        &serde_json::json!({"thinking": {"type": "disabled"}}),
                    )
                } else {
                    with_settings_and_body(
                        &effort,
                        &serde_json::json!({"reasoningEffort": effort}),
                        &serde_json::json!({"thinking": {"type": "enabled"}}),
                    )
                }
            })
            .collect(),
        SupportKind::Toggle => vec![
            body_variant(
                "none",
                &serde_json::json!({"thinking": {"type": "disabled"}}),
            ),
            body_variant(
                "thinking",
                &serde_json::json!({"thinking": {"type": "enabled"}}),
            ),
        ],
        SupportKind::Budget(_) => Vec::new(),
    }
}

fn zai_chat_variants(model: &ModelInfo, package: &str, support: SupportKind<'_>) -> Vec<Variant> {
    let lower = model.model_id.to_ascii_lowercase();
    let (major, minor) = glm_version(&lower).unwrap_or((u64::MAX, 0));
    let coding = package.contains("zai-coding-plan");
    let latest = coding || major == u64::MAX || major > 5 || (major == 5 && minor >= 3);
    let is_52 = major == 5 && minor == 2;
    let unconfigured_effort = matches!(support, SupportKind::Effort(values) if values.is_empty());
    if matches!(support, SupportKind::Toggle) || (!latest && !is_52 && unconfigured_effort) {
        if latest {
            return Vec::new();
        }
        return toggle_variants(
            &serde_json::json!({"thinking": {"type": "disabled"}}),
            &serde_json::json!({"thinking": {"type": "enabled", "clear_thinking": false}}),
        );
    }
    let SupportKind::Effort(values) = support else {
        return Vec::new();
    };
    let defaults: &[&str] = if latest {
        &["low", "high", "max"]
    } else {
        &["high", "max"]
    };
    effort_values(values, defaults)
        .into_iter()
        .map(|value| {
            if value == "none" || value == "minimal" {
                settings_variant(
                    &value,
                    &serde_json::json!({"thinking": {"type": "disabled"}}),
                )
            } else {
                settings_variant(
                    &value,
                    &serde_json::json!({
                        "thinking": {"type": "enabled", "clear_thinking": false},
                        "reasoningEffort": value,
                    }),
                )
            }
        })
        .collect()
}

fn zai_messages_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let lower = model.model_id.to_ascii_lowercase();
    let forced =
        lower.contains("glm-5.3") || lower.contains("glm-5-3") || lower.contains("glm-5p3");
    match support {
        SupportKind::Effort(values) => {
            let defaults: &[&str] = if forced {
                &["low", "high", "max"]
            } else {
                &["high", "max"]
            };
            effort_values(values, defaults)
                .into_iter()
                .map(|effort| {
                    let thinking_type = if effort == "none" || effort == "minimal" {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    settings_variant(
                        &effort,
                        &serde_json::json!({
                            "thinking": {"type": thinking_type},
                            "effort": effort,
                        }),
                    )
                })
                .collect()
        }
        SupportKind::Toggle => {
            if forced {
                Vec::new()
            } else {
                toggle_variants(
                    &serde_json::json!({"thinking": {"type": "disabled"}}),
                    &serde_json::json!({"thinking": {"type": "enabled"}}),
                )
            }
        }
        SupportKind::Budget(_) => Vec::new(),
    }
}

fn moonshot_chat_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    let toggleable =
        id.contains("k2.5") || id.contains("k2-5") || id.contains("k2.6") || id.contains("k2-6");
    let fixed = id.contains("k2-thinking") || id.contains("k2.7-code") || id.contains("k2-7-code");
    if fixed {
        return Vec::new();
    }
    if toggleable
        && (matches!(support, SupportKind::Toggle)
            || matches!(support, SupportKind::Effort(values) if values.is_empty()))
    {
        return toggle_variants(
            &serde_json::json!({"thinking": {"type": "disabled"}}),
            &serde_json::json!({"thinking": {"type": "enabled"}}),
        );
    }
    if toggleable {
        return Vec::new();
    }
    let SupportKind::Effort(values) = support else {
        return Vec::new();
    };
    effort_values(values, &["low", "high", "max"])
        .into_iter()
        .map(|effort| settings_variant(&effort, &serde_json::json!({"reasoningEffort": effort})))
        .collect()
}

fn moonshot_messages_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    let is_k2 = id.contains("kimi-k2") || id.contains("kimi.k2");
    if is_k2 {
        return Vec::new();
    }
    let SupportKind::Effort(values) = support else {
        return Vec::new();
    };
    effort_values(values, &["low", "high", "max"])
        .into_iter()
        .map(|effort| settings_variant(&effort, &serde_json::json!({"effort": effort})))
        .collect()
}

fn alibaba_chat_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    let fixed = id.contains("-thinking")
        || id.contains("qwq")
        || id.contains("deepseek-r1")
        || id.contains("minimax-m2");
    match support {
        SupportKind::Effort(values) => {
            if fixed && values.is_empty() {
                return Vec::new();
            }
            let hosted = id.contains("glm") || id.contains("deepseek");
            let defaults: &[&str] = if hosted {
                &["high", "max"]
            } else {
                &["low", "medium", "xhigh"]
            };
            effort_values(values, defaults)
                .into_iter()
                .map(|effort| {
                    if effort == "none" {
                        settings_variant(&effort, &serde_json::json!({"enableThinking": false}))
                    } else {
                        settings_variant(
                            &effort,
                            &serde_json::json!({"enableThinking": true, "reasoningEffort": effort}),
                        )
                    }
                })
                .collect()
        }
        SupportKind::Toggle => toggle_variants(
            &serde_json::json!({"enableThinking": false}),
            &serde_json::json!({"enableThinking": true}),
        ),
        SupportKind::Budget(budget) => budget_variants(
            model,
            budget,
            64_000,
            |tokens| serde_json::json!({"enableThinking": true, "thinkingBudget": tokens}),
        ),
    }
}

fn alibaba_messages_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    match support {
        SupportKind::Effort(values) => {
            let hosted = id.contains("glm") || id.contains("deepseek");
            let defaults: &[&str] = if hosted {
                &["high", "max"]
            } else {
                &["low", "medium", "xhigh"]
            };
            effort_values(values, defaults)
                .into_iter()
                .map(|effort| {
                    if effort == "none" {
                        settings_variant(
                            &effort,
                            &serde_json::json!({"thinking": {"type": "disabled"}}),
                        )
                    } else {
                        settings_variant(
                            &effort,
                            &serde_json::json!({"thinking": {"type": "enabled"}, "effort": effort}),
                        )
                    }
                })
                .collect()
        }
        SupportKind::Toggle => toggle_variants(
            &serde_json::json!({"thinking": {"type": "disabled"}}),
            &serde_json::json!({"thinking": {"type": "enabled"}}),
        ),
        SupportKind::Budget(budget) => budget_variants(
            model,
            budget,
            64_000,
            |tokens| serde_json::json!({"thinking": {"type": "enabled", "budgetTokens": tokens}}),
        ),
    }
}

fn minimax_messages_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    if id.contains("minimax-m2") || id.contains("minimax.m2") {
        return Vec::new();
    }
    let configurable = matches!(support, SupportKind::Toggle)
        || matches!(support, SupportKind::Effort(values) if values.is_empty());
    if !configurable {
        return Vec::new();
    }
    toggle_variants(
        &serde_json::json!({"thinking": {"type": "disabled"}}),
        &serde_json::json!({"thinking": {"type": "adaptive"}}),
    )
}

fn openrouter_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    match support {
        SupportKind::Effort(values) => effort_values(values, &["low", "medium", "high"])
            .into_iter()
            .map(|effort| {
                settings_variant(
                    &effort,
                    &serde_json::json!({"reasoning": {"effort": effort}}),
                )
            })
            .collect(),
        SupportKind::Toggle => toggle_variants(
            &serde_json::json!({"reasoning": {"enabled": false}}),
            &serde_json::json!({"reasoning": {"enabled": true}}),
        ),
        SupportKind::Budget(budget) => budget_variants(
            model,
            budget,
            model.limit.output.max(1),
            |tokens| serde_json::json!({"reasoning": {"max_tokens": tokens}}),
        ),
    }
}

fn venice_variants(support: SupportKind<'_>) -> Vec<Variant> {
    match support {
        SupportKind::Effort(values) => effort_values(values, &["low", "medium", "high"])
            .into_iter()
            .map(|effort| {
                settings_variant(&effort, &serde_json::json!({"reasoningEffort": effort}))
            })
            .collect(),
        SupportKind::Toggle => toggle_variants(
            &serde_json::json!({"reasoning": {"enabled": false}}),
            &serde_json::json!({"reasoning": {"enabled": true}}),
        ),
        SupportKind::Budget(_) => Vec::new(),
    }
}

fn deepinfra_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    if id.contains("k2.7-code") || id.contains("k2-7-code") {
        return Vec::new();
    }
    match support {
        SupportKind::Effort(values) => effort_values(values, &["low", "medium", "high"])
            .into_iter()
            .map(|effort| {
                settings_variant(&effort, &serde_json::json!({"reasoningEffort": effort}))
            })
            .collect(),
        SupportKind::Toggle => vec![
            body_variant(
                "none",
                &serde_json::json!({"reasoning": {"enabled": false}}),
            ),
            body_variant(
                "thinking",
                &serde_json::json!({"reasoning": {"enabled": true}}),
            ),
        ],
        SupportKind::Budget(_) => Vec::new(),
    }
}

fn workers_ai_variants(support: SupportKind<'_>) -> Vec<Variant> {
    let tpl = |id: &str, thinking: bool| {
        body_variant(
            id,
            &serde_json::json!({"chat_template_kwargs": {"enable_thinking": thinking, "thinking": thinking}}),
        )
    };
    match support {
        SupportKind::Effort(values) => effort_values(values, &["low", "medium", "high"])
            .into_iter()
            .map(|effort| {
                if effort == "none" {
                    tpl(&effort, false)
                } else {
                    settings_variant(&effort, &serde_json::json!({"reasoningEffort": effort}))
                }
            })
            .collect(),
        SupportKind::Toggle => vec![tpl("none", false), tpl("thinking", true)],
        SupportKind::Budget(_) => Vec::new(),
    }
}

fn baseten_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    let tpl = |name: &str, enable: bool| {
        body_variant(
            name,
            &serde_json::json!({"chat_template_args": {"enable_thinking": enable}}),
        )
    };
    match support {
        SupportKind::Effort(values) => {
            let glm = id.contains("glm-5.2") || id.contains("glm-5-2") || id.contains("glm-5p2");
            let ds_pro = id.contains("deepseek-v4-pro-0813");
            effort_values(values, &["low", "medium", "high"])
                .into_iter()
                .map(|effort| {
                    let mut body = Map::new();
                    if glm {
                        body.insert(
                            "chat_template_args".to_string(),
                            serde_json::json!({"enable_thinking": effort != "none"}),
                        );
                    }
                    if ds_pro && effort != "none" {
                        body.insert(
                            "thinking".to_string(),
                            serde_json::json!({"type": "enabled"}),
                        );
                    }
                    Variant {
                        id: effort.clone(),
                        settings: Some(Arc::new(Map::from_iter([(
                            "reasoningEffort".to_string(),
                            Value::String(effort),
                        )]))),
                        headers: None,
                        body: (!body.is_empty()).then_some(Box::new(body)),
                    }
                })
                .collect()
        }
        SupportKind::Toggle => vec![tpl("none", false), tpl("thinking", true)],
        SupportKind::Budget(_) => Vec::new(),
    }
}

fn vertex_chat_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    if !matches!(support, SupportKind::Toggle) {
        return openai_variants(model, support);
    }
    let key = if model.model_id.to_ascii_lowercase().contains("deepseek") {
        "thinking"
    } else {
        "enable_thinking"
    };
    vec![
        body_variant(
            "none",
            &serde_json::json!({"chat_template_kwargs": {key: false}}),
        ),
        body_variant(
            "thinking",
            &serde_json::json!({"chat_template_kwargs": {key: true}}),
        ),
    ]
}

fn nvidia_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let id = model.model_id.to_ascii_lowercase();
    let tpl = |name: &str, kwargs: Value| {
        body_variant(name, &serde_json::json!({"chat_template_kwargs": kwargs}))
    };
    let toggle_key = |key: &str| {
        vec![
            tpl("none", serde_json::json!({key: false})),
            tpl("thinking", serde_json::json!({key: true})),
        ]
    };
    if id.contains("deepseek-v4") {
        return match support {
            SupportKind::Toggle => toggle_key("thinking"),
            SupportKind::Effort(values) => effort_values(values, &["none", "high", "max"])
                .into_iter()
                .map(|effort| {
                    if effort == "none" {
                        tpl(&effort, serde_json::json!({"thinking": false}))
                    } else {
                        tpl(
                            &effort,
                            serde_json::json!({"thinking": true, "reasoning_effort": effort}),
                        )
                    }
                })
                .collect(),
            SupportKind::Budget(_) => Vec::new(),
        };
    }
    if id.contains("kimi-k2.6") || id.contains("kimi-k2-6") {
        return toggle_key("thinking");
    }
    if id.contains("kimi-k3") {
        return match support {
            SupportKind::Toggle => toggle_key("thinking"),
            SupportKind::Effort(values) => effort_values(values, &["low", "high", "max"])
                .into_iter()
                .map(|effort| {
                    settings_variant(&effort, &serde_json::json!({"reasoningEffort": effort}))
                })
                .collect(),
            SupportKind::Budget(_) => Vec::new(),
        };
    }
    if id.contains("minimax-m3") {
        return vec![
            tpl("none", serde_json::json!({"thinking_mode": "disabled"})),
            tpl("thinking", serde_json::json!({"thinking_mode": "enabled"})),
        ];
    }
    if id.contains("glm") {
        return vec![
            tpl("none", serde_json::json!({"enable_thinking": false})),
            tpl(
                "thinking",
                serde_json::json!({"enable_thinking": true, "clear_thinking": false}),
            ),
        ];
    }
    let toggle_only = id.contains("qwen") || id.contains("gemma") || id.contains("nemotron");
    match support {
        SupportKind::Effort(values) if toggle_only && values.is_empty() => toggle_key("enable_thinking"),
        SupportKind::Effort(_) => openai_variants(model, support),
        SupportKind::Toggle => toggle_key("enable_thinking"),
        SupportKind::Budget(budget) => budget_variants(model, budget, model.limit.output.max(1), |tokens| {
            serde_json::json!({"chat_template_kwargs": {"enable_thinking": true}, "reasoning_budget": tokens})
        })
        .into_iter()
        .map(|mut variant| {
            variant.body = variant.settings.take().map(|map| Box::new((*map).clone()));
            variant
        })
        .collect(),
    }
}

fn glm_version(lower: &str) -> Option<(u64, u64)> {
    let idx = lower.find("glm")?;
    let mut rest = lower[idx + 3..].trim_start_matches('-');
    let major_end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    if major_end == 0 {
        return None;
    }
    let major = rest[..major_end].parse::<u64>().ok()?;
    rest = &rest[major_end..];
    let minor = if rest.starts_with(['.', '-', 'p']) {
        let after = &rest[1..];
        let minor_end = after
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(after.len());
        if minor_end > 0 {
            after[..minor_end].parse::<u64>().unwrap_or(0)
        } else {
            0
        }
    } else {
        0
    };
    Some((major, minor))
}

#[derive(Debug)]
struct ClaudeInfo {
    family: Option<String>,
    major: u64,
    minor: u64,
    manual: bool,
    always: bool,
}

fn claude_info(model: &ModelInfo) -> ClaudeInfo {
    let id = model.model_id.to_ascii_lowercase();
    let families = ["opus", "sonnet", "haiku", "fable", "mythos"];
    let mut family = None;
    let mut major = 0;
    let mut minor = 0;
    let tokens: Vec<&str> = id
        .split(|character: char| !character.is_ascii_alphanumeric())
        .collect();
    for (index, token) in tokens.iter().enumerate() {
        if !families.contains(token) {
            continue;
        }
        family = Some((*token).to_string());
        // family-first: claude-sonnet-4-5
        if let Some(number) = tokens
            .get(index + 1)
            .and_then(|token| token.parse::<u64>().ok())
        {
            major = number;
            minor = tokens
                .get(index + 2)
                .and_then(|token| token.parse().ok())
                .unwrap_or(0);
        // version-first: claude-4-5-sonnet
        } else if let (Some(major_token), Some(minor_token)) = (
            index.checked_sub(2).and_then(|index| tokens.get(index)),
            index.checked_sub(1).and_then(|index| tokens.get(index)),
        ) && let (Ok(major_value), Ok(minor_value)) =
            (major_token.parse::<u64>(), minor_token.parse::<u64>())
        {
            major = major_value;
            minor = minor_value;
        }
        break;
    }
    let manual = (major == 3 && minor == 7) || (major == 4 && minor < 6);
    let always = family.as_deref() == Some("fable")
        || family.as_deref() == Some("mythos")
        || id.contains("mythos-preview");
    ClaudeInfo {
        family,
        major,
        minor,
        manual,
        always,
    }
}

fn manual_thinking(model: &ModelInfo) -> Option<Map<String, Value>> {
    let tokens = model.limit.output.min(16_000).saturating_sub(1);
    if tokens < 1024 {
        return None;
    }
    Some(Map::from_iter([(
        "thinking".to_string(),
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("enabled".to_string())),
            ("budgetTokens".to_string(), Value::Number(tokens.into())),
        ])),
    )]))
}

fn toggle_variants(off: &Value, on: &Value) -> Vec<Variant> {
    vec![
        settings_variant("none", off),
        settings_variant("thinking", on),
    ]
}

fn budget_variants(
    model: &ModelInfo,
    budget: (Option<i64>, Option<i64>),
    ceiling: u64,
    build: impl Fn(u64) -> Value,
) -> Vec<Variant> {
    let output_ceiling = model.limit.output.max(1);
    let maximum = budget
        .1
        .map_or(ceiling.saturating_sub(1), unsigned)
        .min(output_ceiling.saturating_sub(1))
        .min(ceiling.saturating_sub(1));
    if maximum == 0 {
        return Vec::new();
    }
    let high = unsigned(budget.0.unwrap_or(0))
        .max(maximum.div_ceil(2))
        .min(maximum);
    vec![
        settings_variant("high", &build(high)),
        settings_variant("max", &build(maximum)),
    ]
}

#[allow(
    clippy::too_many_lines,
    reason = "Claude effort/budget/toggle variants are a single precedence table"
)]
fn anthropic_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    let info = claude_info(model);
    if info.family.is_none() {
        match support {
            SupportKind::Effort(values) => {
                return effort_values(values, &["low", "medium", "high"])
                    .into_iter()
                    .map(|effort| {
                        if effort == "none" {
                            settings_variant(
                                "none",
                                &serde_json::json!({"thinking": {"type": "disabled"}}),
                            )
                        } else {
                            settings_variant(
                                &effort,
                                &serde_json::json!({
                                    "thinking": {"type": "adaptive"},
                                    "effort": effort,
                                }),
                            )
                        }
                    })
                    .collect();
            }
            SupportKind::Toggle => {
                return toggle_variants(
                    &serde_json::json!({"thinking": {"type": "disabled"}}),
                    &serde_json::json!({"thinking": {"type": "adaptive"}}),
                );
            }
            SupportKind::Budget(budget) => {
                return budget_variants(
                    model,
                    budget,
                    32_000,
                    |tokens| serde_json::json!({"thinking": {"type": "enabled", "budgetTokens": tokens}}),
                );
            }
        }
    }

    match support {
        SupportKind::Effort(values) => {
            if info.manual
                && !(info.family.as_deref() == Some("opus") && info.major == 4 && info.minor == 5)
            {
                return anthropic_variants(model, SupportKind::Budget((Some(1024), None)));
            }
            let defaults: Vec<String> = if info.major == 4 && info.minor == 6 {
                ["low", "medium", "high", "max"]
                    .map(str::to_string)
                    .to_vec()
            } else {
                ["low", "medium", "high", "xhigh", "max"]
                    .map(str::to_string)
                    .to_vec()
            };
            let values = if values.is_empty() {
                defaults
            } else {
                values.to_vec()
            };
            let opus45 =
                info.family.as_deref() == Some("opus") && info.major == 4 && info.minor == 5;
            values
                .into_iter()
                .filter(|effort| !(info.always && effort == "none"))
                .map(|effort| {
                    if effort == "none" {
                        return settings_variant(
                            "none",
                            &serde_json::json!({"thinking": {"type": "disabled"}}),
                        );
                    }
                    let mut settings = if opus45 {
                        manual_thinking(model).unwrap_or_else(|| {
                            Map::from_iter([(
                                "thinking".to_string(),
                                serde_json::json!({"type": "enabled"}),
                            )])
                        })
                    } else {
                        Map::from_iter([(
                            "thinking".to_string(),
                            serde_json::json!({"type": "adaptive", "display": "summarized"}),
                        )])
                    };
                    settings.insert("effort".to_string(), Value::String(effort.clone()));
                    with_settings(&effort, settings)
                })
                .collect()
        }
        SupportKind::Toggle => {
            if info.always {
                return Vec::new();
            }
            let thinking = if info.manual {
                manual_thinking(model).unwrap_or_else(|| {
                    Map::from_iter([(
                        "thinking".to_string(),
                        serde_json::json!({"type": "enabled"}),
                    )])
                })
            } else {
                Map::from_iter([(
                    "thinking".to_string(),
                    serde_json::json!({"type": "adaptive", "display": "summarized"}),
                )])
            };
            toggle_variants(
                &serde_json::json!({"thinking": {"type": "disabled"}}),
                &Value::Object(thinking),
            )
        }
        SupportKind::Budget(budget) => budget_variants(
            model,
            budget,
            32_000,
            |tokens| serde_json::json!({"thinking": {"type": "enabled", "budgetTokens": tokens}}),
        ),
    }
}

fn gemini_variants(model: &ModelInfo, support: SupportKind<'_>) -> Vec<Variant> {
    match support {
        SupportKind::Effort(values) => effort_values(values, &["low", "medium", "high"])
            .into_iter()
            .map(|effort| {
                with_settings(
                    &effort,
                    serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingLevel": effort}})
                        .as_object()
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect(),
        SupportKind::Budget(budget) => budget_variants(
            model,
            budget,
            model.limit.output.max(1),
            |tokens| serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": tokens}}),
        ),
        SupportKind::Toggle => toggle_variants(
            &serde_json::json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}),
            &serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}}),
        ),
    }
}
