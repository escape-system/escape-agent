//! Live models.dev refresh: fetch the published catalog, cache it, merge it.
//!
//! The embedded snapshot is a build-time floor, never the source of truth. A
//! running server fetches the published catalog, parses it through the same
//! [`Catalog::from_snapshot`] path the snapshot uses, and merges entries the
//! snapshot does not know yet. Existing entries are never replaced, so a live
//! refresh can add a model but cannot silently reroute or drop one that shipped
//! in the binary.
//!
//! The publisher answers in the models.dev **v1** schema (provider-level `npm`,
//! flat `cost`, no per-model `api` map). [`super::v1`] converts it, including
//! the per-model SDK to wire-protocol mapping, so a newly published Anthropic
//! model is not silently routed to `chat-completions`.
//!
//! Only models this client can actually run are merged: a v1 provider whose SDK
//! maps to [`Protocol::Unsupported`] would otherwise show up in the picker as a
//! row that fails on send.

use std::path::{Path, PathBuf};

use reqwest::Client;

use super::{Catalog, Protocol};
use crate::error::LlmError;

/// Public catalog endpoint, matching upstream's default.
pub const DEFAULT_MODELS_URL: &str = "https://models.dev/api.json";

/// Environment override, matching upstream's `OPENCODE_MODELS_URL`.
pub const MODELS_URL_ENV: &str = "OPENCODE_MODELS_URL";

/// Path override for the on-disk cache.
pub const CACHE_PATH_ENV: &str = "OPENCODE_MODELS_CACHE";

/// Cache file name inside the data directory.
pub const CACHE_FILE: &str = "models-cache.json";

/// Fetch timeout. A refresh that cannot answer in this budget is retried on the
/// next tick instead of stalling shutdown.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Interval between background refreshes.
pub const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The catalog URL this process should use.
#[must_use]
pub fn models_url() -> String {
    std::env::var(MODELS_URL_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS_URL.to_owned())
}

/// Where the fetched catalog is cached between runs.
///
/// Precedence: `OPENCODE_MODELS_CACHE`, `ESCAPE_DATA_DIR`, `OPENCODE_DATA_DIR`,
/// `XDG_DATA_HOME`, `HOME/.local/share`, then the temp directory. The same
/// directory already holds the database, so the cache travels with it.
#[must_use]
pub fn cache_path() -> PathBuf {
    if let Some(path) = std::env::var_os(CACHE_PATH_ENV) {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("ESCAPE_DATA_DIR") {
        return PathBuf::from(dir).join(CACHE_FILE);
    }
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        return PathBuf::from(dir).join(CACHE_FILE);
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("escape-agent").join(CACHE_FILE);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/share/escape-agent")
            .join(CACHE_FILE);
    }
    std::env::temp_dir().join("escape-agent").join(CACHE_FILE)
}

/// Read the cached catalog body, if any.
///
/// A missing or unreadable file is not an error: the caller falls back to the
/// embedded snapshot. Only a body that exists and cannot be parsed fails, so a
/// truncated cache is reported rather than silently ignored.
pub fn read_cache(path: &Path) -> Option<Result<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(body) => Some(Ok(body)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(Err(format!("cannot read {}: {error}", path.display()))),
    }
}

/// Write the cache atomically so kill -9 leaves either the old or the new body.
pub fn write_cache(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, body)
        .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

/// Download the published catalog as raw JSON.
///
/// The raw body is returned so the caller caches exactly what it parses; a
/// cache hit and a network hit therefore produce the same catalog.
pub async fn fetch_live(client: &Client, url: &str) -> Result<String, LlmError> {
    let response = client
        .get(url)
        .timeout(FETCH_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(LlmError::Http {
            status: status.as_u16(),
            message: format!("model catalog fetch failed for {url}"),
            retry_after_ms: None,
        });
    }
    Ok(response.text().await?)
}

/// Parse a published catalog and merge it into `base`.
///
/// Returns the number of models added. A body that does not parse is an error
/// and leaves `base` untouched, so a bad publish can never empty the picker.
pub fn merge_live(base: &mut Catalog, live_json: &str) -> Result<usize, LlmError> {
    let live = Catalog::from_snapshot(live_json)?;
    Ok(merge_catalog(base, &live))
}

/// Merge an already-fetched body into the live cell.
///
/// The body is parsed before the cell is touched, so a malformed publish leaves
/// the running catalog exactly as it was. The revision bumps only when models
/// were actually added.
pub fn apply_live(cell: &super::CatalogCell, body: &str) -> Result<usize, LlmError> {
    let live = Catalog::from_snapshot(body)?;
    let mut added = 0;
    cell.update(|catalog| {
        added = merge_catalog(catalog, &live);
    });
    Ok(added)
}

/// One refresh cycle: fetch the published catalog, cache it, merge it.
///
/// The body is cached exactly as parsed, so an offline start reuses precisely
/// the catalog this run serves. A cache write failure is logged, never fatal:
/// a successful refresh must not be discarded because the disk is read-only.
pub async fn refresh_once(
    cell: &super::CatalogCell,
    client: &Client,
    url: &str,
    cache: &Path,
) -> Result<usize, LlmError> {
    let body = fetch_live(client, url).await?;
    if let Err(error) = write_cache(cache, &body) {
        tracing::warn!(%error, "model catalog cache write failed");
    }
    apply_live(cell, &body)
}

/// Merge `live` into `base`, adding only runnable entries `base` lacks.
///
/// `base` wins on every collision: a model already shipped keeps its snapshot
/// route and pricing, so a refresh can only widen the catalog.
#[must_use]
pub fn merge_catalog(base: &mut Catalog, live: &Catalog) -> usize {
    let mut added = 0;
    for provider in live.providers() {
        let provider_known = base.provider(&provider.id).is_some();
        let models: Vec<_> = live
            .models()
            .into_iter()
            .filter(|model| model.provider_id == provider.id)
            // Only protocols this client can stream; no dead picker rows.
            .filter(|model| model.protocol != Protocol::Unsupported)
            .filter(|model| base.model(&model.provider_id, &model.id).is_none())
            .collect();
        if models.is_empty() {
            continue;
        }
        if !provider_known {
            base.upsert_provider(provider.clone());
        }
        added += models.len();
        base.upsert_provider_models(models);
    }
    if added > 0 {
        base.reindex();
    }
    added
}

/// Spawn the background refresh loop.
///
/// The first fetch waits [`BOOT_DELAY`] so startup never blocks on the network
/// (`docs/15-performance.md` keeps npm/network out of the critical path), then
/// the loop refreshes every [`REFRESH_INTERVAL`]. It lives for the process; a
/// server shutdown ends the process, and a test host simply drops the handle.
#[must_use]
pub fn spawn_refresh_loop(cell: std::sync::Arc<super::CatalogCell>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(client) = Client::builder().build() else {
            return;
        };
        let url = models_url();
        let cache = cache_path();
        let mut first = true;
        loop {
            let delay = if first { BOOT_DELAY } else { REFRESH_INTERVAL };
            first = false;
            tokio::time::sleep(delay).await;
            match refresh_once(&cell, &client, &url, &cache).await {
                Ok(added) if added > 0 => {
                    tracing::info!(added, "model catalog refreshed from {url}");
                }
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, "model catalog refresh failed"),
            }
        }
    })
}

/// Delay before the first background fetch so startup stays offline.
pub const BOOT_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{
      "known": {
        "id": "known",
        "name": "Known",
        "env": [],
        "models": {
          "existing": {
            "id": "existing",
            "type": "chat",
            "name": "Existing",
            "release_date": "2025-01-01",
            "modalities": {"input": ["text"], "output": ["text"]},
            "capabilities": {"tools": {"supported": true}},
            "limit": {"context": 1000, "output": 100},
            "cost": {"text": {"input": {"price": 1}, "output": {"price": 2}}},
            "api": {"chat-completions": {"base_url": "http://known.invalid/v1",
                     "sdk": {"aisdk": "@ai-sdk/openai-compatible"}}}
          }
        }
      }
    }"#;

    /// A v1 publisher body: provider-level npm, per-model provider npm override.
    const LIVE: &str = r#"{
      "known": {
        "id": "known",
        "name": "Known",
        "env": [],
        "npm": "@ai-sdk/openai-compatible",
        "api": "http://known.invalid/v1",
        "models": {
          "existing": {
            "id": "existing",
            "name": "Existing Republished",
            "cost": {"input": 99, "output": 99}
          },
          "fresh": {
            "id": "fresh",
            "name": "Fresh",
            "release_date": "2026-11-16",
            "tool_call": true,
            "temperature": true,
            "cost": {"input": 0, "output": 0},
            "limit": {"context": 1048576, "output": 65536}
          },
          "fresh-anthropic": {
            "id": "fresh-anthropic",
            "name": "Fresh Anthropic-routed",
            "tool_call": true,
            "provider": {"npm": "@ai-sdk/anthropic", "api": "http://known.invalid/anthropic"}
          }
        }
      },
      "brand-new": {
        "id": "brand-new",
        "name": "Brand New",
        "env": [],
        "npm": "@ai-sdk/openai-compatible",
        "api": "http://new.invalid/v1",
        "models": {
          "solo": {
            "id": "solo",
            "name": "Solo",
            "tool_call": true
          }
        }
      },
      "unsupported-only": {
        "id": "unsupported-only",
        "name": "Unsupported Only",
        "env": [],
        "npm": "@openrouter/ai-sdk-provider",
        "models": {
          "weird": {"id": "weird", "name": "Weird"}
        }
      }
    }"#;

    #[test]
    fn merge_adds_new_models_and_keeps_shipped_routes() {
        let mut base = Catalog::from_snapshot(BASE).unwrap();
        let added = merge_live(&mut base, LIVE).unwrap();
        // A v1 publisher body gives every model one of the three protocols this
        // client speaks, so all four new models are runnable and merged.
        assert_eq!(added, 4, "fresh, fresh-anthropic, solo, weird");

        // A shipped model keeps its snapshot pricing (base wins).
        let existing = base.resolve("known/existing").unwrap();
        assert_eq!(existing.model.cost[0].input, 1.0);

        // A new model on a known provider is added with its wire route.
        let fresh = base.resolve("known/fresh").unwrap();
        assert_eq!(fresh.protocol, Protocol::OpenAiChat);
        assert_eq!(fresh.model.name, "Fresh");

        // The per-model Anthropic SDK must not be flattened to chat-completions.
        let anthropic = base.resolve("known/fresh-anthropic").unwrap();
        assert_eq!(anthropic.protocol, Protocol::AnthropicMessages);
        assert_eq!(anthropic.base_url, "http://known.invalid/anthropic");

        // A provider unknown to the snapshot is added when it can run.
        assert_eq!(base.provider("brand-new").unwrap().name, "Brand New");
        assert!(base.resolve("brand-new/solo").is_ok());
    }

    /// A v2-schema live body (no provider-level `npm` anywhere) keeps its route
    /// map, so an unsupported protocol must not reach the picker.
    #[test]
    fn unsupported_protocols_are_not_merged() {
        const V2_LIVE: &str = r#"{
          "odd": {
            "id": "odd",
            "name": "Odd",
            "env": [],
            "models": {
              "dead": {
                "id": "dead",
                "type": "chat",
                "name": "Dead",
                "api": {
                  "systemone": {
                    "base_url": "http://odd.invalid",
                    "sdk": {"aisdk": "@vendor/unknown-transport"}
                  }
                }
              }
            }
          }
        }"#;
        let mut base = Catalog::from_snapshot(BASE).unwrap();
        let added = merge_live(&mut base, V2_LIVE).unwrap();
        assert_eq!(added, 0);
        assert!(base.provider("odd").is_none());
    }

    #[test]
    fn a_bad_body_leaves_the_catalog_untouched() {
        let mut base = Catalog::from_snapshot(BASE).unwrap();
        let before = base.models().len();
        assert!(merge_live(&mut base, "not json").is_err());
        assert_eq!(base.models().len(), before);
    }

    #[test]
    fn cache_round_trips_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models-cache.json");
        assert!(read_cache(&path).is_none());
        write_cache(&path, LIVE).unwrap();
        let body = read_cache(&path).unwrap().unwrap();
        assert_eq!(body, LIVE);
        // No temp file left behind after the rename.
        assert!(!path.with_extension("json.tmp").exists());
    }
}
