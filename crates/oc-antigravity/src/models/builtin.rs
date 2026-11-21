//! Built-in Antigravity Hub and Gemini CLI model definitions.

use crate::constants::{ANTIGRAVITY_ENDPOINT_DAILY, ANTIGRAVITY_PROVIDER_ID, GEMINI_CLI_ENDPOINT};
use oc_llm::{Catalog, ModelInfo};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub(super) enum VariantPreset {
    Effort(&'static [&'static str]),
    Budget { min: u32, max: u32 },
    None,
}

pub(super) struct BuiltinSpec {
    pub(super) id: &'static str,
    pub(super) name: &'static str,
    pub(super) family: &'static str,
    pub(super) context: u64,
    pub(super) output: u64,
    pub(super) base_url: &'static str,
    pub(super) release_date: &'static str,
    pub(super) variants: VariantPreset,
}

pub(super) const BUILTIN_SPECS: &[BuiltinSpec] = &[
    BuiltinSpec {
        id: "antigravity-gemini-4-neon",
        name: "Gemini 4 Neon (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 262_144,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-11-01",
        variants: VariantPreset::Effort(&["low", "medium", "high", "xhigh"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-4-hydrogen",
        name: "Gemini 4 Hydrogen (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 262_144,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-11-01",
        variants: VariantPreset::Effort(&["low", "medium", "high", "xhigh"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-4-argon",
        name: "Gemini 4 Argon (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-10-13",
        variants: VariantPreset::Effort(&["low", "medium", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-3.8-flash",
        name: "Gemini 3.8 Flash (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-09-02",
        variants: VariantPreset::Effort(&["low", "medium", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-3.5-flash-lite",
        name: "Gemini 3.5 Flash Lite (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-07-21",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "antigravity-claude-opus-5-5",
        name: "Claude Opus 5.5 (Antigravity)",
        family: "claude",
        context: 1_000_000,
        output: 128_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-11-01",
        variants: VariantPreset::Effort(&["low", "medium", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-claude-sonnet-5-5",
        name: "Claude Sonnet 5.5 (Antigravity)",
        family: "claude",
        context: 1_000_000,
        output: 128_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-11-01",
        variants: VariantPreset::Effort(&["low", "medium", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gpt-oss-120b-medium",
        name: "GPT-OSS 120B (Medium) (Antigravity)",
        family: "gpt-oss",
        context: 131_072,
        output: 32_768,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-08-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "antigravity-gemini-3.1-pro",
        name: "Gemini 3.1 Pro (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_535,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Effort(&["low", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-3-pro",
        name: "Gemini 3 Pro (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_535,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Effort(&["low", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-3-flash",
        name: "Gemini 3 Flash (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Effort(&["minimal", "low", "medium", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-2.5-pro",
        name: "Gemini 2.5 Pro (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_535,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Effort(&["low", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-gemini-2.5-flash",
        name: "Gemini 2.5 Flash (Antigravity)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Effort(&["low", "medium", "high"]),
    },
    BuiltinSpec {
        id: "antigravity-claude-sonnet-4-6",
        name: "Claude Sonnet 4.6 (Antigravity)",
        family: "claude",
        context: 200_000,
        output: 64_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "antigravity-claude-opus-4-6-thinking",
        name: "Claude Opus 4.6 Thinking (Antigravity)",
        family: "claude",
        context: 200_000,
        output: 64_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Budget {
            min: 8192,
            max: 32768,
        },
    },
    BuiltinSpec {
        id: "antigravity-claude-sonnet-4-5",
        name: "Claude Sonnet 4.5 (Antigravity)",
        family: "claude",
        context: 200_000,
        output: 64_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "antigravity-claude-sonnet-4-5-thinking",
        name: "Claude Sonnet 4.5 Thinking (Antigravity)",
        family: "claude",
        context: 200_000,
        output: 64_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Budget {
            min: 8192,
            max: 32768,
        },
    },
    BuiltinSpec {
        id: "antigravity-claude-opus-4-5-thinking",
        name: "Claude Opus 4.5 Thinking (Antigravity)",
        family: "claude",
        context: 200_000,
        output: 64_000,
        base_url: ANTIGRAVITY_ENDPOINT_DAILY,
        release_date: "2026-01-01",
        variants: VariantPreset::Budget {
            min: 8192,
            max: 32768,
        },
    },
    BuiltinSpec {
        id: "gemini-2.5-flash",
        name: "Gemini 2.5 Flash (Gemini CLI)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: GEMINI_CLI_ENDPOINT,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "gemini-2.5-pro",
        name: "Gemini 2.5 Pro (Gemini CLI)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: GEMINI_CLI_ENDPOINT,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "gemini-3-flash-preview",
        name: "Gemini 3 Flash Preview (Gemini CLI)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: GEMINI_CLI_ENDPOINT,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "gemini-3-pro-preview",
        name: "Gemini 3 Pro Preview (Gemini CLI)",
        family: "gemini",
        context: 1_048_576,
        output: 65_536,
        base_url: GEMINI_CLI_ENDPOINT,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
    BuiltinSpec {
        id: "gemini-3.1-pro-preview",
        name: "Gemini 3.1 Pro Preview (Gemini CLI)",
        family: "gemini",
        context: 1_048_576,
        output: 65_535,
        base_url: GEMINI_CLI_ENDPOINT,
        release_date: "2026-01-01",
        variants: VariantPreset::None,
    },
];

pub(super) fn build_snapshot_catalog(models_map: &Map<String, Value>) -> Option<Catalog> {
    let snapshot = json!({
        "google": {
            "id": ANTIGRAVITY_PROVIDER_ID,
            "name": "Google",
            "env": ["GOOGLE_GENERATIVE_AI_API_KEY"],
            "models": models_map
        }
    });
    let serialized = serde_json::to_string(&snapshot).ok()?;
    Catalog::from_snapshot(&serialized).ok()
}

pub(super) fn spec_to_snapshot_entry(spec: &BuiltinSpec) -> Value {
    let reasoning = match spec.variants {
        VariantPreset::Effort(levels) => json!({ "supported": true, "effort": levels }),
        VariantPreset::Budget { min, max } => {
            json!({ "supported": true, "budget": { "min": min, "max": max } })
        }
        VariantPreset::None => json!({ "supported": false }),
    };

    json!({
        "id": spec.id,
        "type": "chat",
        "name": spec.name,
        "family": spec.family,
        "release_date": spec.release_date,
        "status": "active",
        "modalities": {
            "input": ["text", "image/png", "application/pdf"],
            "output": ["text"]
        },
        "capabilities": {
            "tools": { "supported": true },
            "reasoning": reasoning,
            "temperature": true
        },
        "limit": {
            "context": spec.context,
            "output": spec.output
        },
        "cost": {
            "text": {
                "input": { "price": 0 },
                "output": { "price": 0 }
            }
        },
        "api": {
            "generate-content": {
                "base_url": spec.base_url,
                "sdk": { "aisdk": "@ai-sdk/google" }
            }
        }
    })
}

/// Return the built-in Antigravity Hub and Gemini CLI quota [`ModelInfo`] map for `"google"`.
#[must_use]
pub fn builtin_antigravity_models() -> BTreeMap<String, ModelInfo> {
    let mut map = Map::new();
    for spec in BUILTIN_SPECS {
        map.insert(spec.id.to_owned(), spec_to_snapshot_entry(spec));
    }
    let Some(cat) = build_snapshot_catalog(&map) else {
        return BTreeMap::new();
    };
    cat.models()
        .iter()
        .cloned()
        .map(|m| (m.id.clone(), m))
        .collect()
}
