//! Catalog parsing, resolution, variants, and configuration overlays.

use std::time::Instant;

use oc_llm::{Activation, Catalog, LlmError, ModelStatus, Protocol};
use serde_json::json;

const FIXTURE: &str = r#"{
  "test": {
    "id": "test",
    "name": "Test Provider",
    "env": ["TEST_API_KEY"],
    "models": {
      "alpha": {
        "id": "alpha",
        "type": "chat",
        "name": "Alpha",
        "family": "alpha",
        "release_date": "2025-01-02",
        "status": "beta",
        "modalities": {"input": ["text", "image/png", "application/pdf"], "output": ["text"]},
        "capabilities": {
          "tools": {"supported": true},
          "reasoning": {"supported": true, "effort": ["low", "high"], "toggle": true},
          "temperature": true
        },
        "limit": {"context": 100000, "output": 10000},
        "cost": {
          "text": {
            "input": {"price": 1, "cache": {"read": 0.1, "write": {"5m": 1.25, "1h": 2}}},
            "output": {"price": 2}
          }
        },
        "api": {
          "chat-completions": {
            "base_url": "http://unused.invalid",
            "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
          }
        }
      },
      "claude-sonnet-4-5": {
        "id": "claude-sonnet-4-5",
        "type": "chat",
        "name": "Claude Sonnet 4.5",
        "release_date": "2025-09-29",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {
          "tools": {"supported": true},
          "reasoning": {"supported": true, "budget": {"min": 1024}},
          "temperature": true
        },
        "limit": {"context": 200000, "output": 64000},
        "cost": {"text": {"input": {"price": 3}, "output": {"price": 15}}},
        "api": {
          "messages": {
            "base_url": "http://unused.invalid",
            "sdk": {"aisdk": "@ai-sdk/anthropic"}
          }
        }
      },
      "gemini-pro": {
        "id": "gemini-pro",
        "type": "chat",
        "name": "Gemini Pro",
        "release_date": "2025-02-01",
        "modalities": {"input": ["text"], "output": ["text"]},
        "capabilities": {
          "tools": {"supported": true},
          "reasoning": {"supported": true, "budget": {"min": 0, "max": 24576}},
          "temperature": true
        },
        "limit": {"context": 1000000, "output": 65536},
        "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
        "api": {
          "generate-content": {
            "base_url": "http://unused.invalid",
            "sdk": {"aisdk": "@ai-sdk/google"}
          }
        }
      }
    }
  }
}"#;

fn catalog() -> Catalog {
    Catalog::from_snapshot(FIXTURE).expect("fixture parses")
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn parses_and_resolves_models() {
    let catalog = catalog();
    assert_eq!(catalog.providers().len(), 1);
    assert_eq!(catalog.models().len(), 3);

    let alpha = catalog.resolve("test/alpha").unwrap();
    assert_eq!(alpha.model.name, "Alpha");
    assert_eq!(alpha.model.status, ModelStatus::Beta);
    assert_eq!(alpha.protocol, Protocol::OpenAiChat);
    assert_eq!(alpha.model.limit.context, 100_000);
    assert_close(alpha.model.cost[0].input, 1.0);
    assert_close(alpha.model.cost[0].cache.write, 1.25);

    let claude = catalog.resolve("test/claude-sonnet-4-5").unwrap();
    assert_eq!(claude.protocol, Protocol::AnthropicMessages);
    let gemini = catalog.resolve("test/gemini-pro").unwrap();
    assert_eq!(gemini.protocol, Protocol::Gemini);

    assert_eq!(claude.model.capabilities.input, vec!["text".to_string()]);
    assert_eq!(
        alpha
            .model
            .capabilities
            .input
            .iter()
            .filter(|kind| kind.as_str() == "pdf")
            .count(),
        1
    );
}

#[test]
fn variants_are_derived_and_resolved() {
    let catalog = catalog();
    let alpha = catalog.resolve("test/alpha").unwrap();
    let ids: Vec<&str> = alpha
        .model
        .variants
        .iter()
        .map(|variant| variant.id.as_str())
        .collect();
    assert_eq!(ids, vec!["low", "high"]);
    let variant = catalog.resolve("test/alpha#high").unwrap();
    assert_eq!(variant.variant.unwrap().id, "high");

    let claude = catalog.resolve("test/claude-sonnet-4-5").unwrap();
    let ids: Vec<&str> = claude
        .model
        .variants
        .iter()
        .map(|variant| variant.id.as_str())
        .collect();
    assert_eq!(ids, vec!["high", "max"]);

    let unknown = catalog.resolve("test/alpha#max").unwrap_err();
    assert!(matches!(unknown, LlmError::UnknownVariant { .. }));
    assert!(unknown.to_string().contains("low, high"));
}

#[test]
fn unknown_references_name_candidates() {
    let catalog = catalog();
    let provider = catalog.resolve("tes/alpha").unwrap_err();
    assert!(matches!(provider, LlmError::UnknownProvider { .. }));
    assert!(provider.to_string().contains("test"));

    let model = catalog.resolve("test/alp").unwrap_err();
    assert!(matches!(model, LlmError::UnknownModel { .. }));
    assert!(model.to_string().contains("test/alpha"));

    let syntax = catalog.resolve("nope").unwrap_err();
    assert!(matches!(syntax, LlmError::InvalidReference { .. }));
}

#[test]
fn config_overlay_applies_all_supported_fields() {
    let config = json!({
        "test": {
            "name": "Renamed Provider",
            "package": "@opencode/ai/providers/openai-compatible",
            "activation": "enabled",
            "settings": {"baseURL": "http://127.0.0.1:9/v1", "custom": {"a": 1}},
            "headers": {"x-test": "yes"},
            "body": {"metadata": {"trace": true}},
            "models": {
                "alpha": {
                    "name": "Alpha Custom",
                    "modelID": "alpha-v2",
                    "disabled": true,
                    "limit": {"context": 1234},
                    "headers": {"x-model": "m"},
                    "body": {"temperature": 0.2}
                },
                "new-model": {
                    "name": "New Model",
                    "capabilities": {"tools": true, "input": ["text"], "output": ["text"]},
                    "limit": {"context": 10, "output": 5},
                    "cost": {"input": 1, "output": 2, "cache": {"read": 0.5, "write": 0.25}}
                }
            }
        }
    });
    let mut catalog = catalog();
    catalog.apply_config(&config).unwrap();

    let provider = catalog.provider("test").unwrap();
    assert_eq!(provider.name, "Renamed Provider");
    assert_eq!(provider.activation, Activation::Enabled);
    assert_eq!(provider.headers.as_ref().unwrap()["x-test"], "yes");

    let alpha = catalog.resolve("test/alpha").unwrap();
    assert!(!alpha.model.enabled);
    assert_eq!(alpha.model.name, "Alpha Custom");
    assert_eq!(alpha.model.model_id, "alpha-v2");
    assert_eq!(alpha.model.limit.context, 1234);
    assert_eq!(alpha.base_url, "http://127.0.0.1:9/v1");
    assert_eq!(alpha.headers()["x-model"], "m");
    assert_eq!(alpha.body().get("metadata"), Some(&json!({"trace": true})));
    assert_eq!(alpha.body().get("temperature"), Some(&json!(0.2)));

    let new_model = catalog.resolve("test/new-model").unwrap();
    assert_eq!(new_model.model.name, "New Model");
    assert!(new_model.model.enabled);
    assert_close(new_model.model.cost[0].cache.read, 0.5);
    assert_eq!(new_model.protocol, Protocol::OpenAiChat);
}

#[test]
fn v1_catalog_is_converted() {
    let v1 = r#"{
      "legacy": {
        "id": "legacy",
        "name": "Legacy",
        "env": [],
        "npm": "@ai-sdk/openai-compatible",
        "api": "http://legacy.invalid",
        "models": {
          "old": {
            "id": "old",
            "name": "Old",
            "release_date": "2024-01-01",
            "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
            "tool_call": true,
            "temperature": true,
            "cost": {"input": 1, "output": 2, "cache_read": 0.1},
            "limit": {"context": 8192, "output": 2048},
            "provider": {"npm": "@ai-sdk/openai-compatible"}
          }
        }
      }
    }"#;
    let catalog = Catalog::from_snapshot(v1).unwrap();
    let model = catalog.resolve("legacy/old").unwrap();
    assert_eq!(model.protocol, Protocol::OpenAiChat);
    assert_eq!(model.base_url, "http://legacy.invalid");
    assert_eq!(model.model.limit.context, 8192);
    assert_close(model.model.cost[0].output, 2.0);
    assert_close(model.model.cost[0].cache.read, 0.1);
    let ids: Vec<&str> = model
        .model
        .variants
        .iter()
        .map(|variant| variant.id.as_str())
        .collect();
    assert_eq!(ids, vec!["low", "high"]);
}

#[test]
fn embedded_snapshot_parses_within_a_second() {
    let start = Instant::now();
    let catalog = Catalog::from_snapshot(oc_llm::catalog::EMBEDDED_SNAPSHOT).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 1_000,
        "embedded snapshot took {elapsed:?}"
    );
    assert!(catalog.providers().len() > 100);
    assert!(catalog.models().len() > 1_000);
    let model = catalog.resolve("anthropic/claude-sonnet-4-5").unwrap();
    assert_eq!(model.protocol, Protocol::AnthropicMessages);
    assert!(!model.model.variants.is_empty());
    assert!(catalog.resolve("openai/gpt-5").is_ok());
    assert_eq!(catalog.provider("google").unwrap().name, "Google");
}

#[test]
fn errors_have_no_candidate_panic_on_empty() {
    let catalog = Catalog::from_snapshot(FIXTURE).unwrap();
    let error = catalog.resolve("missing/x").unwrap_err();
    assert!(matches!(error, LlmError::UnknownProvider { .. }));
}
