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
use std::sync::Arc;

use reqwest::Client;
use serde_json::{Map, Value};

use super::variants::{ReasoningSupport, variants_for};
use super::{Activation, Catalog, Cost, CostCache, ModelInfo, Protocol, ProviderInfo};
use crate::error::LlmError;

/// Upstream v2 catalog endpoint with full `capabilities.reasoning` and protocol metadata.
pub const DEFAULT_MODELS_V2_URL: &str = "https://models.opencode.ai/experimental/v2.0/api.json?type=chat,decision,image,video,realtime&include=openai-codex,openai-chatgpt";

/// Legacy v1 fallback catalog endpoint (`models.dev`), also kept as [`DEFAULT_MODELS_URL`].
pub const LEGACY_MODELS_URL: &str = "https://models.dev/api.json";

/// Public catalog endpoint used when `OPENCODE_MODELS_URL` is unset (upstream v2 with v1 fallback).
pub const DEFAULT_MODELS_URL: &str = DEFAULT_MODELS_V2_URL;

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

/// Built-in provider id for the `opencode-go` subscription.
pub const OPENCODE_GO_PROVIDER_ID: &str = "opencode-go";

/// Built-in provider id for Nous Research.
pub const NOUS_PROVIDER_ID: &str = "nous";

/// Default inference base URL for Nous Research (`@ai-sdk/openai-compatible`).
pub const NOUS_BASE_URL: &str = "https://inference-api.nousresearch.com/v1";

/// Default live model-list URL for Nous Research.
pub const NOUS_MODELS_URL: &str = "https://inference-api.nousresearch.com/v1/models";

/// Environment override for the `nous` provider model list.
pub const NOUS_MODELS_URL_ENV: &str = "NOUS_MODELS_URL";

/// Cache file name for fetched Nous Research models inside the data directory.
pub const NOUS_CACHE_FILE: &str = "nous-models-cache.json";

/// Cache file name for the fetched `opencode-go` provider model list.
pub const OPENCODE_GO_CACHE_FILE: &str = "opencode-go-models-cache.json";

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
    env_url_or(OPENCODE_GO_MODELS_URL_ENV, OPENCODE_GO_MODELS_URL)
}

/// The `nous` (Nous Research) provider model-list URL.
#[must_use]
pub fn nous_models_url() -> String {
    env_url_or(NOUS_MODELS_URL_ENV, NOUS_MODELS_URL)
}

fn env_url_or(env_name: &str, default: &str) -> String {
    std::env::var(env_name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// Where the fetched catalog is cached between runs.
///
/// Precedence: `OPENCODE_MODELS_CACHE`, `ESCAPE_DATA_DIR`, `OPENCODE_DATA_DIR`,
/// `XDG_DATA_HOME`, `HOME/.local/share`, then the temp directory. The same
/// directory already holds the database, so the cache travels with it.
#[must_use]
pub fn cache_path() -> PathBuf {
    data_cache_path(Some(CACHE_PATH_ENV), CACHE_FILE)
}

/// Where the fetched Nous Research model list is cached between runs.
#[must_use]
pub fn nous_cache_path() -> PathBuf {
    data_cache_path(None, NOUS_CACHE_FILE)
}

/// Where the fetched `opencode-go` provider model list is cached between runs.
#[must_use]
pub fn opencode_go_cache_path() -> PathBuf {
    data_cache_path(None, OPENCODE_GO_CACHE_FILE)
}

fn data_cache_path(override_env: Option<&str>, file_name: &str) -> PathBuf {
    if let Some(env_name) = override_env
        && let Some(path) = std::env::var_os(env_name)
    {
        return PathBuf::from(path);
    }
    if let Some(dir) = std::env::var_os("ESCAPE_DATA_DIR") {
        return PathBuf::from(dir).join(file_name);
    }
    if let Some(dir) = std::env::var_os("OPENCODE_DATA_DIR") {
        return PathBuf::from(dir).join(file_name);
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("escape-agent").join(file_name);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/share/escape-agent")
            .join(file_name);
    }
    std::env::temp_dir().join("escape-agent").join(file_name)
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
/// The `opencode.ai/zen/go/v1/models` rows are models.dev **v2** records: each
/// one carries `api.{chat-completions,messages,generate-content,responses,…}`
/// with a `sdk.aisdk`/`sdk.opencodeai` package, `cost.text.*`, `modalities`,
/// `capabilities.{tools,reasoning}` and `limit`. Parsing therefore goes through
/// the same conversion the published catalog uses
/// ([`Catalog::from_snapshot`]), so a provider-discovered model keeps its wire
/// route, price band(s) and reasoning variants instead of degrading to
/// `Protocol::Unsupported` with an empty `cost` and no variants.
///
/// Rows whose route is not a protocol this client speaks (`responses`,
/// `systemone`, `images`, `embeddings`) are still returned: they keep
/// `Protocol::Unsupported`, which the transport rejects loudly, and the catalog
/// registry keeps them out of the chat picker. Silently dropping them would
/// hide a real membership decision from the operator.
///
/// `provider_id` overrides the feed's provider key so a caller can attach the
/// list to the id the catalog actually serves. A truncated or structurally
/// wrong body is an error, not an empty result.
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

    // Reuse the published-catalog converter by reshaping the feed into the v2
    // provider document it already knows how to read. Doing it this way keeps
    // one implementation of route selection, modality mapping and price bands;
    // a hand-rolled field copy is what previously dropped `api` and `cost`.
    let mut models_by_id: Map<String, Value> = Map::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || models_by_id.contains_key(id) {
            continue;
        }
        let mut model = row.clone();
        if let Value::Object(map) = &mut model {
            // The feed repeats the id inside the row; the snapshot converter
            // reads it from the object value, and `from_snapshot` fills a
            // missing one from the key, so keep whatever the feed sent.
            map.entry("type".to_owned())
                .or_insert_with(|| Value::String("chat".to_owned()));
        }
        models_by_id.insert(id.to_owned(), model);
    }
    if models_by_id.is_empty() {
        return Err(LlmError::Catalog {
            message: "provider model list contained no usable ids".to_owned(),
        });
    }

    let document = serde_json::json!({
        provider_id: {
            "id": provider_id,
            "name": provider_id,
            "models": Value::Object(models_by_id),
        }
    });
    let catalog = Catalog::from_snapshot(&document.to_string())?;
    let mut models = catalog.models();
    if models.is_empty() {
        return Err(LlmError::Catalog {
            message: "provider model list produced no catalog models".to_owned(),
        });
    }
    // The v2 snapshot converter does not read `canonical_id` (the published
    // catalog carries it per model), so carry it across here: it is the only
    // stable way to correlate a Go id with the canonical vendor id.
    let canonical: std::collections::HashMap<String, String> = rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_str)?;
            let value = row.get("canonical_id").and_then(Value::as_str)?;
            Some((id.to_owned(), value.to_owned()))
        })
        .collect();
    for model in &mut models {
        provider_id.clone_into(&mut model.provider_id);
        if let Some(value) = canonical.get(&model.id) {
            model.canonical = Some(value.clone());
        }
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// Whether a fetched provider model list may take over membership.
///
/// [`MIN_PLAUSIBLE_PROVIDER_MODELS`] (1) is only enough to prove the body is a
/// list at all; taking membership from it on that basis would let a truncated
/// answer withdraw most of a subscription. The gate is therefore **relative**:
/// the fetched list must cover at least half of what the catalog currently
/// serves for that provider. A genuine subscription shrink of more than half at
/// once is not a thing the picker should absorb silently on one fetch.
#[must_use]
pub fn is_plausible_provider_list(fetched: usize, served: usize) -> bool {
    fetched >= MIN_PLAUSIBLE_PROVIDER_MODELS && fetched.saturating_mul(2) >= served
}

/// Ids a provider model list claims, as the membership set for
/// [`Catalog::retain_provider_models`].
#[must_use]
pub fn provider_membership(models: &[ModelInfo]) -> std::collections::HashSet<String> {
    models.iter().map(|model| model.id.clone()).collect()
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
    fetch_opencode_go_models_with_body(client, url)
        .await
        .map(|(_, models)| models)
}

/// Fetch the `opencode-go` model list and keep the raw body for caching.
///
/// The body is returned so the caller can persist exactly what the provider
/// published; re-serializing the parsed models would write a lossy private
/// format and make the cache unreadable by [`parse_provider_models`] later.
pub async fn fetch_opencode_go_models_with_body(
    client: &Client,
    url: &str,
) -> Result<(String, Vec<ModelInfo>), LlmError> {
    let body = fetch_live(client, url).await?;
    let models = parse_provider_models(&body, OPENCODE_GO_PROVIDER_ID)?;
    Ok((body, models))
}

/// [`fetch_opencode_go_models`] against the configured live endpoint.
///
/// The URL still comes from an explicit function instead of an environment
/// read here so the parsing/fetch path stays testable with a fake server.
pub async fn fetch_opencode_go_models_now(client: &Client) -> Result<Vec<ModelInfo>, LlmError> {
    fetch_opencode_go_models(client, &opencode_go_models_url()).await
}

/// Apply a fetched provider model list to `catalog`: upsert what the feed
/// published, then withdraw what it stopped listing.
///
/// This is the single place that decides what a provider list *means*, so the
/// boot path, the background loop and `POST /api/model/refresh` cannot drift
/// apart. Ordering matters: the upsert first makes every published entry carry
/// its own route and prices, then the membership prune removes ids the
/// subscription dropped. The prune is gated on
/// [`is_plausible_provider_list`], so a truncated answer changes nothing.
///
/// Returns `(upserted, removed)`; `removed == 0` when the gate rejected the
/// list, which lets the caller log a skip instead of reporting a shrink.
pub fn apply_provider_models(
    catalog: &mut Catalog,
    provider_id: &str,
    models: Vec<ModelInfo>,
) -> (usize, usize) {
    let served = catalog
        .models()
        .iter()
        .filter(|model| model.provider_id == provider_id)
        .count();
    let upserted = models.len();
    let membership = provider_membership(&models);
    catalog.upsert_provider_models(models);
    if !is_plausible_provider_list(upserted, served) {
        tracing::warn!(
            provider = provider_id,
            fetched = upserted,
            served,
            "provider model list looks truncated; keeping existing membership"
        );
        return (upserted, 0);
    }
    let removed = catalog.retain_provider_models(provider_id, &membership);
    (upserted, removed)
}

/// Built-in `nous` (Nous Research) provider descriptor.
#[must_use]
pub fn default_nous_provider() -> ProviderInfo {
    let mut settings = Map::new();
    settings.insert(
        "baseURL".to_owned(),
        Value::String(NOUS_BASE_URL.to_owned()),
    );
    ProviderInfo {
        id: NOUS_PROVIDER_ID.to_owned(),
        canonical: Some(NOUS_PROVIDER_ID.to_owned()),
        name: "Nous Research".to_owned(),
        activation: Activation::Auto,
        package: "@ai-sdk/openai-compatible".to_owned(),
        compatibility: None,
        settings: Some(Arc::new(settings)),
        headers: None,
        body: None,
        env: vec!["NOUS_API_KEY".to_owned()],
    }
}

/// Starter offline model definitions for `nous` (Nous Research), used before
/// a live fetch from [`NOUS_MODELS_URL`] populates the full catalog cache.
#[must_use]
pub fn default_nous_models() -> Vec<ModelInfo> {
    vec![
        starter_nous_model(&StarterNousModel {
            id: "google/gemini-4-hydrogen",
            name: "Google: Gemini 4 Hydrogen",
            context: 1_048_576,
            output: 943_718,
            input_modalities: &["text", "image", "pdf", "audio", "video"],
            cost: Cost {
                input: 1.5,
                output: 7.5,
                cache: CostCache {
                    read: 0.075,
                    write: 0.0833,
                },
                ..Cost::default()
            },
            efforts: &["low", "medium", "high", "xhigh", "max"],
        }),
        starter_nous_model(&StarterNousModel {
            id: "deepseek/deepseek-v4.1-flash-1017",
            name: "DeepSeek: DeepSeek V4.1 Flash 1017",
            context: 1_048_576,
            output: 393_216,
            input_modalities: &["text", "image"],
            cost: Cost {
                input: 0.15,
                output: 0.60,
                cache: CostCache {
                    read: 0.003,
                    write: 0.0,
                },
                ..Cost::default()
            },
            efforts: &["low", "medium", "high", "max"],
        }),
        starter_nous_model(&StarterNousModel {
            id: "deepseek/deepseek-v4-pro",
            name: "DeepSeek: DeepSeek V4 Pro 0423",
            context: 1_048_576,
            output: 393_216,
            input_modalities: &["text"],
            cost: Cost {
                input: 0.94,
                output: 1.89,
                cache: CostCache {
                    read: 0.08,
                    write: 0.0,
                },
                ..Cost::default()
            },
            efforts: &["low", "medium", "high", "max"],
        }),
        starter_nous_model(&StarterNousModel {
            id: "qwen/qwen4-max",
            name: "Qwen: Qwen4 Max",
            context: 1_000_000,
            output: 131_072,
            input_modalities: &["text", "image", "video"],
            cost: Cost {
                input: 1.425,
                output: 4.275,
                cache: CostCache {
                    read: 0.0713,
                    write: 1.7813,
                },
                ..Cost::default()
            },
            efforts: &["low", "medium", "high", "max"],
        }),
        starter_nous_model(&StarterNousModel {
            id: "qwen/qwen4-max:free",
            name: "Qwen: Qwen4 Max (free)",
            context: 1_000_000,
            output: 131_072,
            input_modalities: &["text", "image", "video"],
            cost: Cost::default(),
            efforts: &["low", "medium", "high", "max"],
        }),
        starter_nous_model(&StarterNousModel {
            id: "anthropic/claude-sonnet-5.5",
            name: "Anthropic: Claude Sonnet 5.5",
            context: 1_000_000,
            output: 128_000,
            input_modalities: &["text", "image"],
            cost: Cost {
                input: 2.0,
                output: 10.0,
                cache: CostCache {
                    read: 0.1,
                    write: 2.5,
                },
                ..Cost::default()
            },
            efforts: &["low", "medium", "high", "xhigh", "max"],
        }),
    ]
}

struct StarterNousModel<'a> {
    id: &'a str,
    name: &'a str,
    context: u64,
    output: u64,
    input_modalities: &'a [&'a str],
    cost: Cost,
    efforts: &'a [&'a str],
}

fn starter_nous_model(spec: &StarterNousModel<'_>) -> ModelInfo {
    let mut model = base_nous_model(spec.id, spec.name);
    model.limit.context = spec.context;
    model.limit.output = spec.output;
    model.capabilities.input = spec
        .input_modalities
        .iter()
        .map(|entry| (*entry).to_owned())
        .collect();
    model.capabilities.output = vec!["text".to_owned()];
    model.capabilities.tools = true;
    model.cost = vec![spec.cost];
    let support = ReasoningSupport {
        supported: true,
        toggle: false,
        effort: Some(
            spec.efforts
                .iter()
                .map(|effort| (*effort).to_owned())
                .collect(),
        ),
        budget: None,
    };
    let package = model
        .package
        .as_deref()
        .unwrap_or("@ai-sdk/openai-compatible");
    model.variants = variants_for(&model, package, Protocol::OpenAiChat, &support);
    model
}

fn base_nous_model(id: &str, name: &str) -> ModelInfo {
    let mut settings = Map::new();
    settings.insert(
        "baseURL".to_owned(),
        Value::String(NOUS_BASE_URL.to_owned()),
    );
    let mut model = ModelInfo::default_for(NOUS_PROVIDER_ID, id);
    name.clone_into(&mut model.name);
    model.package = Some("@ai-sdk/openai-compatible".to_owned());
    model.protocol = Protocol::OpenAiChat;
    model.settings = Some(Arc::new(settings));
    model
}

/// Overlay built-in runtime providers (Nous Research plus frontier ToS-safe
/// `google` Paid Tier and `google-vertex` models) onto `catalog` before user
/// configuration overlays run, keeping cold-start and offline tests complete.
pub fn apply_builtin_providers(catalog: &mut Catalog) {
    catalog.upsert_provider(default_nous_provider());
    catalog.upsert_provider_models(default_nous_models());
    catalog.upsert_provider_models(default_google_frontier_models());
    catalog.upsert_provider_models(default_vertex_frontier_models());
}

fn default_google_frontier_models() -> Vec<ModelInfo> {
    let base_url = "https://generativelanguage.googleapis.com/v1beta";
    let mut neon = gemini_frontier_model(
        "google",
        "gemini-4-neon",
        "Gemini 4 Neon",
        base_url,
        Protocol::Gemini,
        &["text", "image", "pdf", "audio", "video"],
        Cost {
            input: 3.0,
            output: 15.0,
            cache: CostCache {
                read: 0.15,
                write: 0.0,
            },
            ..Cost::default()
        },
        &["low", "medium", "high", "xhigh"],
    );
    neon.family = Some("gemini".to_owned());

    let carbon = gemini_frontier_model(
        "google",
        "gemini-4-carbon",
        "Gemini 4 Carbon",
        base_url,
        Protocol::Gemini,
        &["text", "image", "pdf", "audio", "video"],
        Cost {
            input: 4.0,
            output: 20.0,
            cache: CostCache {
                read: 0.20,
                write: 0.0,
            },
            ..Cost::default()
        },
        &["low", "medium", "high", "xhigh"],
    );
    let hydrogen = gemini_frontier_model(
        "google",
        "gemini-4-hydrogen",
        "Gemini 4 Hydrogen",
        base_url,
        Protocol::Gemini,
        &["text", "image", "pdf", "audio", "video"],
        Cost {
            input: 1.50,
            output: 7.50,
            cache: CostCache {
                read: 0.075,
                write: 0.0,
            },
            ..Cost::default()
        },
        &["low", "medium", "high", "xhigh"],
    );
    let antigravity_managed = gemini_frontier_model(
        "google",
        "antigravity-preview-09-2026",
        "Antigravity Managed Agent (Interactions API)",
        base_url,
        Protocol::GeminiInteractions,
        &["text", "image", "pdf"],
        Cost {
            input: 3.0,
            output: 15.0,
            cache: CostCache {
                read: 0.15,
                write: 0.0,
            },
            ..Cost::default()
        },
        &["low", "medium", "high", "xhigh"],
    );
    vec![neon, carbon, hydrogen, antigravity_managed]
}

fn default_vertex_frontier_models() -> Vec<ModelInfo> {
    let gemini_base = "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/publishers/google";
    let anthropic_base = "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/publishers/anthropic/models";
    vec![
        gemini_frontier_model(
            "google-vertex",
            "gemini-4-neon",
            "Vertex: Gemini 4 Neon",
            gemini_base,
            Protocol::Gemini,
            &["text", "image", "pdf", "audio", "video"],
            Cost {
                input: 3.0,
                output: 15.0,
                cache: CostCache {
                    read: 0.15,
                    write: 0.0,
                },
                ..Cost::default()
            },
            &["low", "medium", "high", "xhigh"],
        ),
        gemini_frontier_model(
            "google-vertex",
            "gemini-4-carbon",
            "Vertex: Gemini 4 Carbon",
            gemini_base,
            Protocol::Gemini,
            &["text", "image", "pdf", "audio", "video"],
            Cost {
                input: 4.0,
                output: 20.0,
                cache: CostCache {
                    read: 0.20,
                    write: 0.0,
                },
                ..Cost::default()
            },
            &["low", "medium", "high", "xhigh"],
        ),
        gemini_frontier_model(
            "google-vertex",
            "gemini-4-hydrogen",
            "Vertex: Gemini 4 Hydrogen",
            gemini_base,
            Protocol::Gemini,
            &["text", "image", "pdf", "audio", "video"],
            Cost {
                input: 1.50,
                output: 7.50,
                cache: CostCache {
                    read: 0.075,
                    write: 0.0,
                },
                ..Cost::default()
            },
            &["low", "medium", "high", "xhigh"],
        ),
        gemini_frontier_model(
            "google-vertex",
            "claude-opus-5-5@default",
            "Vertex: Claude Opus 5.5",
            anthropic_base,
            Protocol::AnthropicMessages,
            &["text", "image", "pdf"],
            Cost {
                input: 4.0,
                output: 20.0,
                cache: CostCache {
                    read: 0.20,
                    write: 5.0,
                },
                ..Cost::default()
            },
            &["low", "medium", "high", "xhigh", "max"],
        ),
        gemini_frontier_model(
            "google-vertex",
            "claude-sonnet-5-5@default",
            "Vertex: Claude Sonnet 5.5",
            anthropic_base,
            Protocol::AnthropicMessages,
            &["text", "image", "pdf"],
            Cost {
                input: 2.0,
                output: 10.0,
                cache: CostCache {
                    read: 0.10,
                    write: 2.5,
                },
                ..Cost::default()
            },
            &["low", "medium", "high", "xhigh", "max"],
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn gemini_frontier_model(
    provider_id: &str,
    id: &str,
    name: &str,
    base_url: &str,
    protocol: Protocol,
    input_modalities: &[&str],
    cost: Cost,
    efforts: &[&str],
) -> ModelInfo {
    let mut settings = Map::new();
    settings.insert("baseURL".to_owned(), Value::String(base_url.to_owned()));
    if provider_id == "google-vertex" && protocol == Protocol::AnthropicMessages {
        settings.insert("vertex".to_owned(), Value::Bool(true));
    }
    let mut model = ModelInfo::default_for(provider_id, id);
    name.clone_into(&mut model.name);
    model.protocol = protocol;
    model.package = Some(match protocol {
        Protocol::AnthropicMessages => "@ai-sdk/google-vertex/anthropic".to_owned(),
        _ => "@ai-sdk/google".to_owned(),
    });
    model.limit.context = 1_048_576;
    model.limit.output = 131_072;
    model.capabilities.tools = true;
    model.capabilities.input = input_modalities.iter().map(|m| (*m).to_owned()).collect();
    model.capabilities.output = vec!["text".to_owned()];
    model.cost = vec![cost];
    model.settings = Some(Arc::new(settings));
    let support = ReasoningSupport {
        supported: true,
        toggle: false,
        effort: Some(efforts.iter().map(|e| (*e).to_owned()).collect()),
        budget: None,
    };
    let package = model.package.as_deref().unwrap_or("@ai-sdk/google");
    model.variants = variants_for(&model, package, protocol, &support);
    model
}

/// Parse Nous Research's OpenRouter-compatible `/v1/models` payload into
/// catalog models for [`NOUS_PROVIDER_ID`].
///
/// Prices published by Nous (`pricing.prompt`, `pricing.completion`,
/// `pricing.input_cache_read`, `pricing.input_cache_write`) are denominated in
/// USD per token and converted here to USD per 1M tokens for [`Cost`].
pub fn parse_nous_models(body: &str) -> Result<Vec<ModelInfo>, LlmError> {
    let value: Value = serde_json::from_str(body).map_err(|error| LlmError::Catalog {
        message: format!("nous model list is not JSON: {error}"),
    })?;
    let Some(rows) = value.get("data").and_then(Value::as_array) else {
        return Err(LlmError::Catalog {
            message: "nous model list has no `data` array".to_owned(),
        });
    };
    if rows.len() < MIN_PLAUSIBLE_PROVIDER_MODELS {
        return Err(LlmError::Catalog {
            message: format!(
                "nous model list has {} models (< {MIN_PLAUSIBLE_PROVIDER_MODELS})",
                rows.len()
            ),
        });
    }
    let models: Vec<ModelInfo> = rows.iter().filter_map(parse_nous_model).collect();
    if models.is_empty() {
        return Err(LlmError::Catalog {
            message: "nous model list contained no usable ids".to_owned(),
        });
    }
    Ok(models)
}

fn parse_nous_model(row: &Value) -> Option<ModelInfo> {
    let id = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(id);
    let mut model = base_nous_model(id, name);
    model.canonical = row
        .get("canonical_slug")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(created) = row.get("created").and_then(Value::as_i64) {
        model.time.released = created.saturating_mul(1000);
    }
    apply_nous_capabilities(&mut model, id, row);
    if let Some(pricing) = row.get("pricing").and_then(Value::as_object)
        && let Some(cost) = parse_nous_cost(pricing)
    {
        model.cost = vec![cost];
    }
    Some(model)
}

fn apply_nous_capabilities(model: &mut ModelInfo, id: &str, row: &Value) {
    let arch = row.get("architecture");
    if arch
        .and_then(|a| a.get("modality"))
        .and_then(Value::as_str)
        .is_some_and(|modality| modality.contains("->embeddings"))
    {
        model.kind = Some("embedding".to_owned());
    }
    if let Some(inputs) = arch
        .and_then(|a| a.get("input_modalities"))
        .and_then(Value::as_array)
    {
        let parsed = normalize_nous_modalities(inputs);
        if !parsed.is_empty() {
            model.capabilities.input = parsed;
        }
    }
    if let Some(outputs) = arch
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array)
    {
        let parsed = normalize_nous_modalities(outputs);
        if !parsed.is_empty() {
            model.capabilities.output = parsed;
        }
    }
    let top = row.get("top_provider");
    if let Some(context) = row
        .get("context_length")
        .and_then(Value::as_u64)
        .or_else(|| {
            top.and_then(|t| t.get("context_length"))
                .and_then(Value::as_u64)
        })
    {
        model.limit.context = context;
    }
    if let Some(output) = top
        .and_then(|t| t.get("max_completion_tokens"))
        .and_then(Value::as_u64)
    {
        model.limit.output = output;
    }
    if let Some(params) = row.get("supported_parameters").and_then(Value::as_array) {
        let param_names: Vec<&str> = params.iter().filter_map(Value::as_str).collect();
        model.capabilities.tools =
            param_names.contains(&"tools") || param_names.contains(&"tool_choice");
        if param_names.contains(&"reasoning") || param_names.contains(&"reasoning_effort") {
            let efforts = if id.contains("gemini") || id.contains("claude") {
                vec!["low", "medium", "high", "xhigh", "max"]
            } else {
                vec!["low", "medium", "high", "max"]
            };
            let support = ReasoningSupport {
                supported: true,
                toggle: false,
                effort: Some(efforts.into_iter().map(str::to_owned).collect()),
                budget: None,
            };
            let package = model
                .package
                .as_deref()
                .unwrap_or("@ai-sdk/openai-compatible");
            model.variants = variants_for(model, package, Protocol::OpenAiChat, &support);
        }
    }
}

fn normalize_nous_modalities(entries: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries.iter().filter_map(Value::as_str) {
        let normalized = match entry {
            "file" => "pdf",
            other => other,
        };
        if !normalized.is_empty() && !out.iter().any(|seen| seen == normalized) {
            out.push(normalized.to_owned());
        }
    }
    out
}

fn parse_nous_cost(pricing: &Map<String, Value>) -> Option<Cost> {
    let input = price_per_million(pricing.get("prompt"));
    let output = price_per_million(pricing.get("completion"));
    let cache_read = price_per_million(pricing.get("input_cache_read"));
    let cache_write = price_per_million(pricing.get("input_cache_write"));
    if input.is_none() && output.is_none() && cache_read.is_none() && cache_write.is_none() {
        return None;
    }
    Some(Cost {
        input: input.unwrap_or(0.0),
        output: output.unwrap_or(0.0),
        cache: CostCache {
            read: cache_read.unwrap_or(0.0),
            write: cache_write.unwrap_or(0.0),
        },
        ..Cost::default()
    })
}

fn price_per_million(raw: Option<&Value>) -> Option<f64> {
    let per_token = match raw? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (per_token.is_finite() && per_token >= 0.0).then_some(per_token * 1_000_000.0)
}

/// Fetch the live Nous Research model list, returning `(raw_body, models)` so
/// callers can cache the payload alongside upserting into [`Catalog`].
pub async fn fetch_nous_models_with_body(
    client: &Client,
    url: &str,
) -> Result<(String, Vec<ModelInfo>), LlmError> {
    let body = fetch_live(client, url).await?;
    let models = parse_nous_models(&body)?;
    Ok((body, models))
}

/// Fetch the live Nous Research model list from an explicit URL.
pub async fn fetch_nous_models(client: &Client, url: &str) -> Result<Vec<ModelInfo>, LlmError> {
    fetch_nous_models_with_body(client, url)
        .await
        .map(|(_, models)| models)
}

/// [`fetch_nous_models_with_body`] against [`nous_models_url`].
pub async fn fetch_nous_models_now(client: &Client) -> Result<(String, Vec<ModelInfo>), LlmError> {
    fetch_nous_models_with_body(client, &nous_models_url()).await
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

/// Short-lived HTTP client without idle connection pooling, used for infrequent
/// background refreshes so sockets and TLS state tear down right after the tick.
#[must_use]
pub fn transient_client() -> Client {
    Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap_or_else(|_| Client::new())
}

async fn fetch_catalog_body(client: &Client, url: &str) -> Result<String, LlmError> {
    match fetch_live(client, url).await {
        Ok(body) => Ok(body),
        Err(error) if url == DEFAULT_MODELS_V2_URL => {
            tracing::debug!(%error, "v2 model catalog fetch failed; falling back to models.dev v1");
            fetch_live(client, LEGACY_MODELS_URL).await
        }
        Err(error) => Err(error),
    }
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
    let body = fetch_catalog_body(client, url)
        .await
        .map_err(|e| e.to_string())?;
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
        let url = models_url();
        let cache = cache_path();
        let mut first = true;
        loop {
            let delay = if first { BOOT_DELAY } else { REFRESH_INTERVAL };
            first = false;
            tokio::time::sleep(delay).await;
            let Ok(client) = Client::builder().pool_max_idle_per_host(0).build() else {
                continue;
            };
            match fetch_catalog_body(&client, &url).await {
                Ok(body) => {
                    drop(client);
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
    fn provider_model_list_keeps_route_cost_and_variants() {
        // Rows shaped like the real `opencode.ai/zen/go/v1/models` feed, which
        // publishes models.dev v2 records: a route with an SDK package, a
        // `cost.text` band, modalities and reasoning support. A hand-rolled
        // field copy used to drop `api` and `cost`, so every
        // provider-discovered model landed as `Unsupported`, unpriced and with
        // no variants.
        let body = r#"{
          "object": "list",
          "data": [
            {
              "id": "hy4-flash-free", "type": "chat", "name": "Hy4 flash Free",
              "canonical_id": "tencent/hy4-flash", "release_date": "2026-11-16",
              "modalities": {"input": ["text", "image/png"], "output": ["text"]},
              "capabilities": {
                "tools": {"supported": true},
                "reasoning": {"supported": true, "effort": ["none", "high"]}
              },
              "limit": {"context": 1048576, "output": 65536},
              "cost": {"text": {"input": {"price": 0, "cache": {"read": 0}},
                                "output": {"price": 0}}},
              "api": {"chat-completions": {
                "base_url": "https://opencode.ai/inference/go/openai/v1",
                "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
              }}
            },
            {
              "id": "qwen4-max", "type": "chat", "name": "Qwen4 Max",
              "limit": {"context": 1000000, "output": 262144},
              "cost": {"text": {"input": {"price": 1.5}, "output": {"price": 4.5}}},
              "api": {"messages": {
                "base_url": "https://opencode.ai/inference/go/anthropic/v1",
                "sdk": {"aisdk": "@ai-sdk/anthropic"}
              }}
            },
            {"name": "missing id"},
            {"id": "no-route", "name": "No Route"}
          ]
        }"#;
        let models = parse_provider_models(body, "opencode-go").unwrap();
        assert_eq!(
            models.len(),
            2,
            "rows without ids or without a route are skipped"
        );
        let hy4 = models
            .iter()
            .find(|model| model.id == "hy4-flash-free")
            .expect("hy4-flash-free is published");
        assert_eq!(hy4.model_id, "hy4-flash-free");
        assert_eq!(hy4.provider_id, "opencode-go");
        assert_eq!(hy4.name, "Hy4 flash Free");
        assert_eq!(hy4.canonical.as_deref(), Some("tencent/hy4-flash"));
        assert_eq!(hy4.limit.context, 1_048_576);
        assert_eq!(hy4.limit.output, 65_536);
        assert!(hy4.capabilities.tools);
        // MIME modalities collapse to the broad kinds the picker understands.
        assert_eq!(hy4.capabilities.input, vec!["text", "image"]);
        // A free tier really is priced at zero, not merely missing a price.
        assert_eq!(hy4.cost[0].input, 0.0);
        assert_eq!(hy4.cost[0].output, 0.0);
        // Reasoning efforts become pickable variants.
        assert_eq!(
            hy4.variants
                .iter()
                .map(|variant| variant.id.as_str())
                .collect::<Vec<_>>(),
            vec!["none", "high"]
        );
        // The Anthropic-routed model keeps its Messages protocol and price.
        let qwen = models
            .iter()
            .find(|model| model.id == "qwen4-max")
            .expect("qwen4-max is published");
        assert_eq!(qwen.protocol, Protocol::AnthropicMessages);
        assert_eq!(qwen.cost[0].input, 1.5);
        assert_eq!(qwen.cost[0].output, 4.5);
    }

    #[test]
    fn opencode_go_hungerg_free_exposes_max_effort() {
        let body = r#"{
          "object": "list",
          "data": [{
            "id": "hungerg-free", "type": "chat", "name": "HungerG Free",
            "capabilities": {
              "tools": {"supported": true},
              "reasoning": {"supported": true}
            },
            "limit": {"context": 1000000, "output": 65536},
            "api": {"chat-completions": {
              "base_url": "https://opencode.ai/inference/go/openai/v1",
              "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
            }}
          }]
        }"#;
        let models = parse_provider_models(body, OPENCODE_GO_PROVIDER_ID).unwrap();
        assert_eq!(models.len(), 1);
        let hungerg = &models[0];
        assert_eq!(hungerg.id, "hungerg-free");
        assert_eq!(hungerg.protocol, Protocol::OpenAiChat);
        assert_eq!(hungerg.variants.len(), 1);
        assert_eq!(hungerg.variants[0].id, "max");
        assert_eq!(
            hungerg.variants[0]
                .settings
                .as_ref()
                .and_then(|settings| settings.get("reasoningEffort")),
            Some(&Value::String("max".to_owned()))
        );

        let mut catalog = Catalog::default();
        catalog.upsert_provider(ProviderInfo {
            id: OPENCODE_GO_PROVIDER_ID.to_owned(),
            canonical: None,
            name: OPENCODE_GO_PROVIDER_ID.to_owned(),
            activation: Activation::Auto,
            package: "@ai-sdk/openai-compatible".to_owned(),
            compatibility: None,
            settings: None,
            headers: None,
            body: None,
            env: Vec::new(),
        });
        apply_provider_models(&mut catalog, OPENCODE_GO_PROVIDER_ID, models);
        let resolved = catalog
            .resolve("opencode-go/hungerg-free#max")
            .expect("maximum HungerG effort resolves");
        assert_eq!(resolved.model_id(), "hungerg-free");
        assert_eq!(
            resolved.settings().get("reasoningEffort"),
            Some(&Value::String("max".to_owned()))
        );
    }

    #[test]
    fn membership_prune_withdraws_only_unlisted_ids() {
        let mut catalog = Catalog::default();
        for id in ["kept-a", "kept-b", "dropped", "kept-c"] {
            catalog.upsert_provider_models(vec![ModelInfo::default_for("opencode-go", id)]);
        }
        catalog.upsert_provider_models(vec![ModelInfo::default_for("other", "untouched")]);

        let feed = feed_models(&["kept-a", "kept-b", "kept-c"]);
        let (upserted, removed) = apply_provider_models(&mut catalog, "opencode-go", feed);

        assert_eq!(upserted, 3);
        assert_eq!(removed, 1);
        assert!(catalog.model("opencode-go", "dropped").is_none());
        assert!(catalog.model("opencode-go", "kept-a").is_some());
        // Another provider's models are never touched by this prune.
        assert!(catalog.model("other", "untouched").is_some());
    }

    #[test]
    fn a_truncated_provider_list_never_prunes() {
        let mut catalog = Catalog::default();
        for index in 0..10 {
            catalog.upsert_provider_models(vec![ModelInfo::default_for(
                "opencode-go",
                &format!("m{index}"),
            )]);
        }

        let feed = feed_models(&["m0", "m1"]);
        let (upserted, removed) = apply_provider_models(&mut catalog, "opencode-go", feed);

        // The entries it did publish are still upserted, but membership is
        // unchanged: half the subscription disappearing on one fetch is a bad
        // response, not a decision.
        assert_eq!(upserted, 2);
        assert_eq!(removed, 0);
        assert!(catalog.model("opencode-go", "m9").is_some());
    }

    #[test]
    fn provider_list_plausibility_is_relative_to_what_is_served() {
        assert!(is_plausible_provider_list(5, 10), "half is enough");
        assert!(is_plausible_provider_list(10, 10));
        assert!(!is_plausible_provider_list(4, 10));
        // An empty catalog cannot regress, but an empty fetch is still rejected.
        assert!(is_plausible_provider_list(1, 0));
        assert!(!is_plausible_provider_list(0, 0));
    }

    fn feed_models(ids: &[&str]) -> Vec<ModelInfo> {
        ids.iter()
            .map(|id| ModelInfo::default_for("opencode-go", id))
            .collect()
    }

    #[test]
    fn a_truncated_provider_model_list_is_rejected() {
        assert!(parse_provider_models("{}", "opencode-go").is_err());
        assert!(parse_provider_models(r#"{"data":[]}"#, "opencode-go").is_err());
        assert!(parse_provider_models("not json", "opencode-go").is_err());
        // Ids that yield no usable route are not a list this client can serve.
        assert!(parse_provider_models(r#"{"data":[{"id":"route-less"}]}"#, "opencode-go").is_err());
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
          "id": "hy4-flash-free", "type": "chat", "name": "Hy4 flash Free",
          "canonical_id": "tencent/hy4-flash", "release_date": "2026-11-16",
          "modalities": {"input": ["text", "image/png"], "output": ["text"]},
          "capabilities": {"tools": {"supported": true}},
          "limit": {"context": 1048576, "output": 65536},
          "cost": {"text": {"input": {"price": 0}, "output": {"price": 0}}},
          "api": {"chat-completions": {
            "base_url": "https://opencode.ai/inference/go/openai/v1",
            "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
          }}
        },
        {
          "id": "another-go-model", "type": "chat", "name": "Another",
          "api": {"chat-completions": {
            "base_url": "https://opencode.ai/inference/go/openai/v1",
            "sdk": {"aisdk": "@ai-sdk/openai-compatible"}
          }}
        }
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
        let hy4 = models
            .iter()
            .find(|model| model.id == "hy4-flash-free")
            .expect("hy4-flash-free is published");
        assert_eq!(hy4.provider_id, "opencode-go");
        assert_eq!(hy4.canonical.as_deref(), Some("tencent/hy4-flash"));
        assert_eq!(hy4.limit.context, 1_048_576);
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

    const NOUS_MODELS: &str = r#"{
      "data": [
        {
          "id": "google/gemini-4-hydrogen",
          "canonical_slug": "google/gemini-4-hydrogen-202611",
          "name": "Google: Gemini 4 Hydrogen",
          "created": 1795000000,
          "context_length": 1048576,
          "architecture": {
            "modality": "text+image+file->text",
            "input_modalities": ["text", "image", "file"],
            "output_modalities": ["text"]
          },
          "pricing": {
            "prompt": "0.0000015000",
            "completion": "0.0000075000",
            "input_cache_read": "0.0000000750",
            "input_cache_write": "0.0000000833"
          },
          "top_provider": {"context_length": 1048576, "max_completion_tokens": 943718},
          "supported_parameters": ["reasoning", "reasoning_effort", "tools", "tool_choice"]
        },
        {
          "id": "qwen/qwen4-max:free",
          "name": "Qwen: Qwen4 Max (free)",
          "context_length": 1000000,
          "pricing": {"prompt": "0", "completion": "0"},
          "supported_parameters": ["tools"]
        },
        {
          "id": "perplexity/pplx-embed-v2-context-0.6b",
          "name": "Embed",
          "context_length": 32000,
          "architecture": {"modality": "text->embeddings"},
          "supported_parameters": []
        }
      ]
    }"#;

    #[test]
    fn parse_nous_models_converts_pricing_modalities_and_reasoning() {
        let models = parse_nous_models(NOUS_MODELS).unwrap();
        assert_eq!(models.len(), 3);

        let gemini = &models[0];
        assert_eq!(gemini.provider_id, "nous");
        assert_eq!(gemini.id, "google/gemini-4-hydrogen");
        assert_eq!(gemini.model_id, "google/gemini-4-hydrogen");
        assert_eq!(gemini.kind.as_deref(), Some("chat"));
        assert_eq!(gemini.protocol, Protocol::OpenAiChat);
        assert_eq!(gemini.limit.context, 1_048_576);
        assert_eq!(gemini.limit.output, 943_718);
        assert_eq!(gemini.capabilities.input, vec!["text", "image", "pdf"]);
        assert!(gemini.capabilities.tools);
        assert!(gemini.variants.iter().any(|v| v.id == "xhigh"));
        assert_eq!(gemini.cost.len(), 1);
        assert!((gemini.cost[0].input - 1.5).abs() < 1e-6);
        assert!((gemini.cost[0].output - 7.5).abs() < 1e-6);
        assert!((gemini.cost[0].cache.read - 0.075).abs() < 1e-6);

        let free = &models[1];
        assert_eq!(free.cost[0].input, 0.0);
        assert!(free.variants.is_empty());

        let embed = &models[2];
        assert_eq!(embed.kind.as_deref(), Some("embedding"));
        assert!(!embed.capabilities.tools);
    }

    #[test]
    fn apply_builtin_providers_registers_nous_without_network() {
        let mut catalog = Catalog::default();
        apply_builtin_providers(&mut catalog);
        let provider = catalog.provider("nous").expect("nous provider");
        assert_eq!(provider.name, "Nous Research");
        assert_eq!(provider.env, vec!["NOUS_API_KEY".to_owned()]);
        let resolved = catalog.resolve("nous/google/gemini-4-hydrogen").unwrap();
        assert_eq!(resolved.base_url, NOUS_BASE_URL);
        assert_eq!(resolved.protocol, Protocol::OpenAiChat);
        assert!((resolved.model.cost[0].input - 1.5).abs() < 1e-6);
    }
}
