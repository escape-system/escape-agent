//! Reasoning variant synthesis per protocol.

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
            toggle: source.toggle,
            effort: source.effort.clone(),
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

pub(super) fn variants_for(
    model: &ModelInfo,
    protocol: Protocol,
    support: &ReasoningSupport,
) -> Vec<Variant> {
    match protocol {
        Protocol::OpenAiChat => openai_variants(model, support),
        Protocol::AnthropicMessages => anthropic_variants(model, support),
        Protocol::Gemini => gemini_variants(model, support),
        Protocol::Unsupported => Vec::new(),
    }
}

fn settings_variant(id: &str, settings: &Value) -> Variant {
    Variant {
        id: id.to_string(),
        settings: settings.as_object().cloned(),
        headers: None,
        body: None,
    }
}

fn with_settings(id: &str, inner: Map<String, Value>) -> Variant {
    settings_variant(id, &Value::Object(inner))
}

fn openai_variants(_model: &ModelInfo, support: &ReasoningSupport) -> Vec<Variant> {
    let Some(values) = support.effort.as_ref() else {
        return Vec::new();
    };
    let values = if values.is_empty() {
        vec!["low".to_string(), "medium".to_string(), "high".to_string()]
    } else {
        values.clone()
    };
    values
        .into_iter()
        .map(|effort| {
            with_settings(
                &effort,
                Map::from_iter([("reasoningEffort".to_string(), Value::String(effort.clone()))]),
            )
        })
        .collect()
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
fn anthropic_variants(model: &ModelInfo, support: &ReasoningSupport) -> Vec<Variant> {
    let info = claude_info(model);
    if info.family.is_none() {
        if let Some(values) = &support.effort {
            let values = if values.is_empty() {
                vec!["low".to_string(), "medium".to_string(), "high".to_string()]
            } else {
                values.clone()
            };
            return values
                .into_iter()
                .map(|effort| {
                    with_settings(
                        &effort,
                        Map::from_iter([(
                            "thinking".to_string(),
                            serde_json::json!({"type": "adaptive"}),
                        )]),
                    )
                })
                .collect();
        }
        if support.toggle {
            return toggle_variants(
                &serde_json::json!({"thinking": {"type": "disabled"}}),
                &serde_json::json!({"thinking": {"type": "adaptive"}}),
            );
        }
        return Vec::new();
    }

    if let Some(values) = &support.effort {
        if info.manual
            && !(info.family.as_deref() == Some("opus") && info.major == 4 && info.minor == 5)
        {
            return anthropic_variants(
                model,
                &ReasoningSupport {
                    budget: Some((Some(1024), None)),
                    ..ReasoningSupport::default()
                },
            );
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
            values.clone()
        };
        let opus45 = info.family.as_deref() == Some("opus") && info.major == 4 && info.minor == 5;
        return values
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
            .collect();
    }

    if support.toggle {
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
        return toggle_variants(
            &serde_json::json!({"thinking": {"type": "disabled"}}),
            &Value::Object(thinking),
        );
    }

    if let Some(budget) = support.budget {
        return budget_variants(
            model,
            budget,
            32_000,
            |tokens| serde_json::json!({"thinking": {"type": "enabled", "budgetTokens": tokens}}),
        );
    }
    Vec::new()
}

fn gemini_variants(model: &ModelInfo, support: &ReasoningSupport) -> Vec<Variant> {
    if let Some(values) = &support.effort {
        let values = if values.is_empty() {
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        } else {
            values.clone()
        };
        return values
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
            .collect();
    }
    if let Some(budget) = support.budget {
        return budget_variants(
            model,
            budget,
            model.limit.output.max(1),
            |tokens| serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": tokens}}),
        );
    }
    if support.toggle {
        return toggle_variants(
            &serde_json::json!({"thinkingConfig": {"includeThoughts": false, "thinkingBudget": 0}}),
            &serde_json::json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": -1}}),
        );
    }
    Vec::new()
}
