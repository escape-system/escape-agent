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
use serde_json::Value;

use super::{Catalog, ModelInfo};
use crate::error::LlmError;

/// Public catalog endpoint, matching upstream's default.
pub const DEFAULT_MODELS_URL: &str = "https://models.dev/api.json";

/// Environment override, matching upstream's `OPENCODE_MODELS_URL`.
pub const MODELS_URL_ENV: &str = "OPENCODE_MODELS_URL";

/// Path override for the on-disk cache.
pub const CACHE_PATH_ENV: &str = "OPENCODE_MODELS_CACHE";

/// Cache file name inside the data directory.
pub const CACHE_FILE: &str = "models-cache.json";

/// Default origin for the `opencode-go` provider's OpenAI-style model list.
pub const OPENCODE_GO_MODELS_URL: &str = "https://opencode.ai/zen/go/v1/models";

/// Environment override for the `opencode-go` provider model list.
pub const OPENCODE_GO_MODELS_URL_ENV: &str = "OPENCODE_GO_MODELS_URL";

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

/// A provider list below this many models is a broken response.
pub const MIN_PLAUSIBLE_PROVIDER_MODELS: usize = 1;

/// The catalog URL this process should use.
#[must_use]
pub fn models_url() -> String {
    std::env::var(MODELS_URL_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS_URL.to_owned())
}

/// The `opencode-go` provider model-list URL.
///
/// The endpoint answers an OpenAI-style `{object:"list", data:[...]}` document,
/// including `hy4-flash-free`, without credentials (verified live 2026-11-20).
#[must_use]
pub fn opencode_go_models_url() -> String {
    std::env::var(OPENCODE_GO_MODELS_URL_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OPENCODE_GO_MODELS_URL.to_owned())
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

/// Parse an OpenAI-style `{object:"list", data:[{id, name, ...}]}` body into
/// catalog models for one provider.
///
/// Only fields the provider actually publishes are used: `id` (required),
/// `name`, `canonical_id`, `release_date`, `limit.context`/`limit.output`, and
/// `capabilities.tools.supported`. Unknown ids are skipped rather than faked.
/// A truncated or structurally wrong body is an error, not an empty result.
pub fn parse_provider_models(body: &str, provider_id: &str) -> Result<Vec<ModelInfo>, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(|error| LlmError::Catalog {
        message: format!("provider model list is not JSON: {error}"),
    })?;
    let Some(rows) = value.get("data").and_then(Value::as_array) else {
        return Err(LlmError::Catalog {
            message: "provider model list has no `data` array".to_owned(),
        });
    };
    if rows.len() < MIN_PLAUSIBLE_PROVIDER_MODELS {
        return Err(LlmError::Catalog {
            message: format!(
                "provider model list has {} models (< {MIN_PLAUSIBLE_PROVIDER_MODELS})",
                rows.len()
            ),
        });
    }
    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let mut model = ModelInfo::default_for(provider_id, id);
        if let Some(name) = row.get("name").and_then(Value::as_str) {
            name.clone_into(&mut model.name);
        }
        model.canonical = row
            .get("canonical_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let limit = row.get("limit");
        if let Some(context) = limit.and_then(|l| l.get("context")).and_then(Value::as_u64) {
            model.limit.context = context;
        }
        if let Some(output) = limit.and_then(|l| l.get("output")).and_then(Value::as_u64) {
            model.limit.output = output;
        }
        if row
            .get("capabilities")
            .and_then(|c| c.get("tools"))
            .and_then(|t| t.get("supported"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            model.capabilities.tools = false;
        }
        if let Some(release) = row.get("release_date").and_then(Value::as_str) {
            model.time.released = super::parse_release_date(release);
        }
        models.push(model);
    }
    if models.is_empty() {
        return Err(LlmError::Catalog {
            message: "provider model list contained no usable ids".to_owned(),
        });
    }
    Ok(models)
}

/// Fetch the live `opencode-go` model list and parse it into catalog models.
///
/// On-demand only: this does NOT swap the catalog cell or change the server's
/// refresh loop, which still needs server/overlay wiring outside this crate's
/// closure owner. The URL is explicit so callers (and tests with a fake axum
/// server) never have to mutate process-global environment variables.
pub async fn fetch_opencode_go_models(
    client: &Client,
    url: &str,
) -> Result<Vec<ModelInfo>, LlmError> {
    let body = fetch_live(client, url).await?;
    parse_provider_models(&body, "opencode-go")
}

/// [`fetch_opencode_go_models`] against the configured live endpoint.
///
/// The URL still comes from an explicit function instead of an environment
/// read here so the parsing/fetch path stays testable with a fake server.
pub async fn fetch_opencode_go_models_now(client: &Client) -> Result<Vec<ModelInfo>, LlmError> {
    fetch_opencode_go_models(client, &opencode_go_models_url()).await
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

/// Build an HTTP client for an explicit, on-demand catalog refresh.
///
/// The background loop constructs its own client (and skips a tick if that
/// fails); a manual refresh needs a client too but has no dependency on a
/// caller that can supply one, so oc-server reuses this helper instead of
/// taking a direct `reqwest` dependency.
#[must_use]
pub fn client() -> Client {
    Client::new()
}

/// Fetch, cache and apply the published catalog exactly once.
///
/// The URL is explicit: callers must not rely on process-global environment
/// mutation (edition 2024 forbids `set_var` and tests would race). The body is
/// cached only after a successful fetch, and `apply` is never called when the
/// fetch failed — it owns parse, plausibility, overlays, the cell swap and the
/// event publish, and returns an error when the body was rejected.
pub async fn refresh_now<F>(
    client: &Client,
    url: &str,
    cache: &Path,
    apply: F,
) -> Result<(), String>
where
    F: Fn(&str) -> Result<(), String>,
{
    let body = fetch_live(client, url).await.map_err(|e| e.to_string())?;
    write_cache(cache, &body)?;
    apply(&body).map_err(|error| {
        tracing::warn!(%error, %url, "manual model catalog publish rejected");
        error
    })
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

    #[test]
    fn provider_model_list_parses_only_published_fields() {
        let body = r#"{
          "object": "list",
          "data": [
            {
              "id": "hy4-flash-free", "name": "Hy4 flash Free",
              "canonical_id": "tencent/hy4-flash", "release_date": "2026-11-16",
              "capabilities": {"tools": {"supported": true}},
              "limit": {"context": 1048576, "output": 65536}
            },
            {
              "id": "no-tools", "name": "No Tools",
              "capabilities": {"tools": {"supported": false}}
            },
            {"name": "missing id"}
          ]
        }"#;
        let models = parse_provider_models(body, "opencode-go").unwrap();
        assert_eq!(models.len(), 2, "rows without ids are skipped");
        assert_eq!(models[0].id, "hy4-flash-free");
        assert_eq!(models[0].model_id, "hy4-flash-free");
        assert_eq!(models[0].provider_id, "opencode-go");
        assert_eq!(models[0].name, "Hy4 flash Free");
        assert_eq!(models[0].canonical.as_deref(), Some("tencent/hy4-flash"));
        assert_eq!(models[0].limit.context, 1_048_576);
        assert_eq!(models[0].limit.output, 65_536);
        assert!(models[0].capabilities.tools);
        assert!(!models[1].capabilities.tools);
    }

    #[test]
    fn a_truncated_provider_model_list_is_rejected() {
        assert!(parse_provider_models("{}", "opencode-go").is_err());
        assert!(parse_provider_models(r#"{"data":[]}"#, "opencode-go").is_err());
        assert!(parse_provider_models("not json", "opencode-go").is_err());
    }

    #[tokio::test]
    async fn refresh_now_fetches_caches_and_applies_once() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE);
        let router = axum::Router::new().route("/live", axum::routing::get(|| async { LIVE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/live", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, router).into_future());

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&calls);
        refresh_now(&client(), &url, &cache, |body| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(body, LIVE);
            Ok(())
        })
        .await
        .unwrap();
        server.abort();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(read_cache(&cache).unwrap().unwrap(), LIVE);
    }

    const GO_MODELS: &str = r#"{
      "object": "list",
      "data": [
        {
          "id": "hy4-flash-free", "name": "Hy4 flash Free",
          "canonical_id": "tencent/hy4-flash", "release_date": "2026-11-16",
          "capabilities": {"tools": {"supported": true}},
          "limit": {"context": 1048576, "output": 65536}
        },
        {"id": "another-go-model", "name": "Another"}
      ]
    }"#;

    #[tokio::test]
    async fn fetch_opencode_go_models_fetches_and_parses_with_an_explicit_url() {
        let router =
            axum::Router::new().route("/go/v1/models", axum::routing::get(|| async { GO_MODELS }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/go/v1/models", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, router).into_future());

        let models = fetch_opencode_go_models(&client(), &url).await.unwrap();
        server.abort();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "hy4-flash-free");
        assert_eq!(models[0].provider_id, "opencode-go");
        assert_eq!(models[0].canonical.as_deref(), Some("tencent/hy4-flash"));
        assert_eq!(models[0].limit.context, 1_048_576);
    }

    #[tokio::test]
    async fn fetch_opencode_go_models_rejects_a_bad_response() {
        let router = axum::Router::new().route(
            "/bad",
            axum::routing::get(|| async { (axum::http::StatusCode::BAD_GATEWAY, "bad") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/bad", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, router).into_future());

        let result = fetch_opencode_go_models(&client(), &url).await;
        server.abort();

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn refresh_now_never_applies_a_failed_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join(CACHE_FILE);
        let router = axum::Router::new().route(
            "/bad",
            axum::routing::get(|| async { (axum::http::StatusCode::BAD_REQUEST, "bad") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/bad", listener.local_addr().unwrap());
        let server = tokio::spawn(axum::serve(listener, router).into_future());

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&calls);
        let result = refresh_now(&client(), &url, &cache, |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await;
        server.abort();

        assert!(result.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(read_cache(&cache).is_none());
    }
}
