//! Module wiring and shared fixtures for the observer-host tests.

use std::path::Path;
use std::sync::Arc;

use oc_config::{ConfigService, LoadOptions};
use oc_store::{NewGoal, Store};
use oc_types::{ServerInfo, ServerPaths};

use crate::AppState;

mod judge;
mod notifier;
mod wiring;

const CATALOG: &str = r#"{
      "test": {
        "id": "test",
        "name": "Test Provider",
        "env": [],
        "models": {
          "alpha": {
            "id": "alpha",
            "type": "chat",
            "name": "Alpha",
            "release_date": "2025-01-02",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "temperature": true},
            "limit": {"context": 100000, "output": 10000},
            "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
            "api": {
              "chat-completions": {
                "base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
              }
            }
          }
        }
      }
    }"#;

/// Like [`CATALOG`], but the provider declares an environment credential.
const ENV_CATALOG: &str = r#"{
      "test": {
        "id": "test",
        "name": "Test Provider",
        "env": ["OC_OBSERVER_TEST_API_KEY"],
        "models": {
          "alpha": {
            "id": "alpha",
            "type": "chat",
            "name": "Alpha",
            "release_date": "2025-01-02",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}, "temperature": true},
            "limit": {"context": 100000, "output": 10000},
            "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
            "api": {
              "chat-completions": {
                "base_url": "http://unused.invalid",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
              }
            }
          }
        }
      }
    }"#;

fn config_at(directory: &Path, content: &str) -> ConfigService {
    ConfigService::load(
        LoadOptions::new(directory)
            .with_env("HOME", directory.to_string_lossy())
            .with_env("USERPROFILE", directory.to_string_lossy())
            .with_env(
                "XDG_CONFIG_HOME",
                directory.join("config").to_string_lossy(),
            )
            .with_env(
                "ProgramData",
                directory.join("programdata").to_string_lossy(),
            )
            .with_env(
                "OPENCODE_CONFIG",
                directory.join("missing.json").to_string_lossy(),
            )
            .with_env("OPENCODE_CONFIG_CONTENT", content),
    )
    .expect("config loads")
}

fn state_at(directory: &Path) -> AppState {
    AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        directory.to_path_buf(),
    )
}

async fn store_at(directory: &Path) -> Arc<Store> {
    Arc::new(
        Store::open(directory.join("db.sqlite"))
            .await
            .expect("store opens"),
    )
}

async fn insert_goal(store: &Store, id: &str, root_id: &str) {
    store
        .set_goal(NewGoal {
            id: id.to_owned(),
            root_id: root_id.to_owned(),
            objective: "ship it".to_owned(),
            criteria: vec!["tests pass".to_owned()],
            model: None,
            time_created: oc_types::now_millis(),
        })
        .await
        .expect("goal inserted");
}
