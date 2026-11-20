//! Live models.dev refresh: fetch the published catalog, cache it, make it the
//! catalog the server serves.
//!
//! Upstream treats models.dev as a **dynamic** source, not a build artifact: the
//! published catalog is authoritative, it is cached on disk, and a refresh
//! announces itself with `models-dev.refreshed`. The embedded snapshot is only
//! the cold-start floor for a machine that has never fetched (this repo keeps
//! startup offline-first, `docs/15-performance.md`), so the first successful
//! fetch replaces it.
//!
//! Replacing rather than merging is deliberate: an additive merge would keep a
//! model upstream renamed, repriced or withdrew forever, which is exactly the
//! staleness the live feed exists to remove. The cost of replacement is that a
//! broken publish could empty the picker, so callers gate the swap on
//! [`is_plausible`]; a publish that loses almost everything is a bad response,
//! not an upstream decision.

use std::path::{Path, PathBuf};

use reqwest::Client;

use super::Catalog;
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

/// Delay before the first background fetch so startup stays offline.
pub const BOOT_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// A published catalog below this many models is a broken response, not an
/// upstream decision; the live catalog has thousands.
pub const MIN_PLAUSIBLE_MODELS: usize = 100;

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
/// A missing file is not an error: the caller falls back to the embedded
/// snapshot. An unreadable or unparseable cache is reported, not ignored.
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

/// Parse a published body (models.dev v1 or v2) into a standalone catalog.
///
/// A body that does not parse is an error; the caller keeps the current
/// catalog, so a bad publish can never empty the picker.
pub fn parse_live(body: &str) -> Result<Catalog, LlmError> {
    Catalog::from_snapshot(body)
}

/// Whether a catalog is complete enough to replace the one being served.
#[must_use]
pub fn is_plausible(catalog: &Catalog) -> bool {
    catalog.models().len() >= MIN_PLAUSIBLE_MODELS
}

/// Replace the cell's catalog with a parsed published body.
///
/// Returns the model count of the new generation. Callers that need overlays
/// (config providers, Antigravity discovery) build the final [`Catalog`] first
/// and call [`super::CatalogCell::replace`] directly.
pub fn replace_live(cell: &super::CatalogCell, body: &str) -> Result<usize, LlmError> {
    let live = parse_live(body)?;
    let count = live.models().len();
    cell.replace(live);
    Ok(count)
}

/// Spawn the background refresh loop: fetch, cache, apply.
///
/// `apply` owns the interpretation of a fetched body (parse, re-apply overlays,
/// swap the cell, publish `models-dev.refreshed`) and returns an error when the
/// body was rejected, so the loop can log without knowing any of that layering.
/// The loop lives for the process; shutdown ends the process.
#[must_use]
pub fn spawn_refresh_loop<F>(apply: F) -> tokio::task::JoinHandle<()>
where
    F: Fn(&str) -> Result<(), String> + Send + 'static,
{
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
            match fetch_live(&client, &url).await {
                Ok(body) => {
                    if let Err(error) = write_cache(&cache, &body) {
                        tracing::warn!(%error, "model catalog cache write failed");
                    }
                    match apply(&body) {
                        Ok(()) => tracing::info!("model catalog refreshed from {url}"),
                        Err(error) => tracing::warn!(%error, "model catalog publish rejected"),
                    }
                }
                Err(error) => tracing::debug!(%error, "model catalog refresh failed"),
            }
        }
    })
}

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

    const LIVE: &str = r#"{
      "known": {
        "id": "known",
        "name": "Known",
        "env": [],
        "npm": "@ai-sdk/openai-compatible",
        "api": "http://known.invalid/v1",
        "models": {
          "fresh": {
            "id": "fresh",
            "name": "Fresh",
            "release_date": "2026-11-16",
            "tool_call": true,
            "temperature": true,
            "cost": {"input": 0, "output": 0},
            "limit": {"context": 1048576, "output": 65536}
          }
        }
      }
    }"#;

    #[test]
    fn replace_is_authoritative() {
        let cell = super::super::CatalogCell::new(Catalog::from_snapshot(BASE).unwrap());
        assert!(cell.current().resolve("known/existing").is_ok());

        let count = replace_live(&cell, LIVE).unwrap();
        assert_eq!(count, 1);

        // The withdrawn entry is gone and the published one is served: the live
        // catalog replaces the old generation instead of merging into it.
        assert!(cell.current().resolve("known/existing").is_err());
        assert!(cell.current().resolve("known/fresh").is_ok());
    }

    #[test]
    fn a_bad_body_leaves_the_catalog_untouched() {
        let cell = super::super::CatalogCell::new(Catalog::from_snapshot(BASE).unwrap());
        let before = cell.models().len();
        assert!(replace_live(&cell, "not json").is_err());
        assert_eq!(cell.models().len(), before);
        assert!(cell.current().resolve("known/existing").is_ok());
    }

    #[test]
    fn a_tiny_publish_is_not_plausible() {
        let tiny = Catalog::from_snapshot(BASE).unwrap();
        assert!(!is_plausible(&tiny));
    }

    #[test]
    fn cache_round_trips_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        assert!(read_cache(&path).is_none());
        write_cache(&path, LIVE).unwrap();
        assert_eq!(read_cache(&path).unwrap().unwrap(), LIVE);
        // No temp file left behind after the rename.
        assert!(!path.with_extension("json.tmp").exists());
    }
}
