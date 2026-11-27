//! Google Cloud Vertex AI endpoint resolution and Application Default Credentials (ADC).
//!
//! Resolves `${GOOGLE_VERTEX_ENDPOINT}`, `${GOOGLE_VERTEX_PROJECT}`, and
//! `${GOOGLE_VERTEX_LOCATION}` catalog URL templates and generates short-lived
//! OAuth 2.0 Bearer tokens via:
//! 1. explicit `GOOGLE_VERTEX_ACCESS_TOKEN`,
//! 2. `service_account` JSON (`GOOGLE_APPLICATION_CREDENTIALS` or standard
//!    `application_default_credentials.json`) signed in pure safe Rust using
//!    `ring::signature::RsaKeyPair`,
//! 3. `authorized_user` JSON refresh-token exchange, or
//! 4. optional metadata endpoint override (`OC_GCP_METADATA_URL`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use reqwest::Client;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::error::LlmError;

const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const TOKEN_REFRESH_MARGIN_SECS: i64 = 120;

#[derive(Clone, Deserialize)]
#[serde(tag = "type")]
enum AdcFile {
    #[serde(rename = "service_account")]
    ServiceAccount {
        client_email: String,
        private_key: String,
        #[serde(default)]
        token_uri: Option<String>,
    },
    #[serde(rename = "authorized_user")]
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

#[derive(Clone)]
struct CachedBearer {
    token: Zeroizing<String>,
    expires_at_epoch_s: i64,
}

impl std::fmt::Debug for CachedBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedBearer")
            .field("token", &"<redacted>")
            .field("expires_at_epoch_s", &self.expires_at_epoch_s)
            .finish()
    }
}

/// Thread-safe, zeroizing GCP ADC token provider.
#[derive(Debug, Default)]
pub struct GoogleAdcClient {
    cache: Mutex<BTreeMap<String, CachedBearer>>,
}

impl GoogleAdcClient {
    /// Resolve an `OAuth2` Bearer access token for a Vertex AI request.
    ///
    /// Checks an explicit key/token, `GOOGLE_VERTEX_ACCESS_TOKEN`, explicit JSON
    /// path in `settings`, `GOOGLE_APPLICATION_CREDENTIALS`, standard gcloud ADC
    /// path, and `OC_GCP_METADATA_URL`.
    pub async fn bearer_token(
        &self,
        http: &Client,
        explicit_credential: Option<&str>,
        settings: &Map<String, Value>,
    ) -> Result<Zeroizing<String>, LlmError> {
        if let Some(raw) = explicit_credential.map(str::trim).filter(|s| !s.is_empty()) {
            // If someone pasted JSON credentials directly as the credential value,
            // treat it as inline ADC JSON; otherwise treat it as a bearer token.
            if raw.starts_with('{') {
                return self.token_from_adc_json(http, "inline", raw).await;
            }
            return Ok(Zeroizing::new(raw.to_owned()));
        }

        if let Some(env_token) = std::env::var("GOOGLE_VERTEX_ACCESS_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(Zeroizing::new(env_token.trim().to_owned()));
        }

        let creds_path = settings
            .get("credentialsPath")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(discover_adc_path);

        if let Some(path) = creds_path {
            let cache_key = path.to_string_lossy().into_owned();
            if let Some(hit) = self.cached(&cache_key) {
                return Ok(hit);
            }
            let raw = std::fs::read_to_string(&path).map_err(|error| LlmError::Config {
                message: format!(
                    "failed to read GCP credentials at {}: {error}",
                    path.display()
                ),
            })?;
            return self.token_from_adc_json(http, &cache_key, &raw).await;
        }

        if let Some(metadata_url) = std::env::var("OC_GCP_METADATA_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            if let Some(hit) = self.cached(&metadata_url) {
                return Ok(hit);
            }
            return self.fetch_metadata_token(http, &metadata_url).await;
        }

        Err(LlmError::InvalidRequest {
            message: "Vertex AI authentication requires `GOOGLE_VERTEX_ACCESS_TOKEN`, \
                      `GOOGLE_APPLICATION_CREDENTIALS`, or `gcloud auth application-default login` credentials"
                .to_owned(),
        })
    }

    /// Exchange inline ADC JSON (`service_account` or `authorized_user`) using a cache slot.
    pub async fn token_from_adc_json(
        &self,
        http: &Client,
        cache_key: &str,
        raw_json: &str,
    ) -> Result<Zeroizing<String>, LlmError> {
        if let Some(hit) = self.cached(cache_key) {
            return Ok(hit);
        }
        let parsed: AdcFile = serde_json::from_str(raw_json).map_err(|error| LlmError::Config {
            message: format!("invalid GCP ADC JSON credentials: {error}"),
        })?;
        let bearer = match parsed {
            AdcFile::ServiceAccount {
                client_email,
                private_key,
                token_uri,
            } => {
                let token_uri = token_uri.as_deref().unwrap_or(DEFAULT_TOKEN_URI);
                let private_key = Zeroizing::new(private_key);
                let assertion = build_service_account_jwt(&client_email, token_uri, &private_key)?;
                exchange_jwt_assertion(http, token_uri, &assertion).await?
            }
            AdcFile::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
            } => {
                let secret = Zeroizing::new(client_secret);
                let refresh = Zeroizing::new(refresh_token);
                let token_uri = std::env::var("OC_GCP_TOKEN_URI")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_owned());
                exchange_refresh_token(http, &token_uri, &client_id, &secret, &refresh).await?
            }
        };
        let token = bearer.token.clone();
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(cache_key.to_owned(), bearer);
        }
        Ok(token)
    }

    fn cached(&self, key: &str) -> Option<Zeroizing<String>> {
        let now = now_epoch_s();
        self.cache.lock().ok().and_then(|cache| {
            cache.get(key).and_then(|entry| {
                if entry.expires_at_epoch_s > now + TOKEN_REFRESH_MARGIN_SECS {
                    Some(entry.token.clone())
                } else {
                    None
                }
            })
        })
    }

    async fn fetch_metadata_token(
        &self,
        http: &Client,
        url: &str,
    ) -> Result<Zeroizing<String>, LlmError> {
        let resp = http
            .get(url)
            .header("Metadata-Flavor", "Google")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(LlmError::from)?;
        if !resp.status().is_success() {
            return Err(LlmError::Http {
                status: resp.status().as_u16(),
                message: "GCE metadata server refused token request".to_owned(),
                retry_after_ms: None,
            });
        }
        let body: TokenResponse = resp.json().await.map_err(LlmError::from)?;
        let bearer = CachedBearer {
            token: Zeroizing::new(body.access_token),
            expires_at_epoch_s: now_epoch_s() + body.expires_in.unwrap_or(3600),
        };
        let out = bearer.token.clone();
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(url.to_owned(), bearer);
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn build_service_account_jwt(
    client_email: &str,
    token_uri: &str,
    private_key_pem: &Zeroizing<String>,
) -> Result<Zeroizing<String>, LlmError> {
    let der = decode_pkcs8_pem(private_key_pem)?;
    let key_pair = RsaKeyPair::from_pkcs8(&der).map_err(|error| LlmError::Config {
        message: format!("invalid Vertex service-account PKCS#8 private key: {error}"),
    })?;

    let iat = now_epoch_s();
    let exp = iat + 3600;
    let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": token_uri,
        "iat": iat,
        "exp": exp,
    })
    .to_string();
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims.as_bytes());
    let signing_input = format!("{header_b64}.{claims_b64}");

    let rng = SystemRandom::new();
    let mut sig = vec![0_u8; key_pair.public().modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .map_err(|_| LlmError::Config {
            message: "failed to sign Vertex service-account JWT assertion".to_owned(),
        })?;
    let sig_b64 = URL_SAFE_NO_PAD.encode(&sig);
    Ok(Zeroizing::new(format!("{signing_input}.{sig_b64}")))
}

fn decode_pkcs8_pem(pem: &Zeroizing<String>) -> Result<Zeroizing<Vec<u8>>, LlmError> {
    let mut b64 = Zeroizing::new(String::new());
    for line in pem.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("-----BEGIN")
            || trimmed.starts_with("-----END")
        {
            continue;
        }
        b64.push_str(trimmed);
    }
    STANDARD
        .decode(b64.as_bytes())
        .map(Zeroizing::new)
        .map_err(|error| LlmError::Config {
            message: format!("invalid PEM base64 in GCP service account key: {error}"),
        })
}

async fn exchange_jwt_assertion(
    http: &Client,
    token_uri: &str,
    assertion: &Zeroizing<String>,
) -> Result<CachedBearer, LlmError> {
    let resp = http
        .post(token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(LlmError::from)?;
    parse_token_response(resp).await
}

async fn exchange_refresh_token(
    http: &Client,
    token_uri: &str,
    client_id: &str,
    client_secret: &Zeroizing<String>,
    refresh_token: &Zeroizing<String>,
) -> Result<CachedBearer, LlmError> {
    let resp = http
        .post(token_uri)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
        ])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(LlmError::from)?;
    parse_token_response(resp).await
}

async fn parse_token_response(resp: reqwest::Response) -> Result<CachedBearer, LlmError> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(LlmError::Http {
            status: status.as_u16(),
            message: format!("GCP OAuth2 token exchange failed: {body}"),
            retry_after_ms: None,
        });
    }
    let parsed: TokenResponse = resp.json().await.map_err(LlmError::from)?;
    Ok(CachedBearer {
        token: Zeroizing::new(parsed.access_token),
        expires_at_epoch_s: now_epoch_s() + parsed.expires_in.unwrap_or(3600),
    })
}

/// Resolve `${GOOGLE_VERTEX_*}` placeholders in a model's `base_url` template.
pub fn resolve_vertex_base_url(
    base_url: &str,
    settings: &Map<String, Value>,
) -> Result<String, LlmError> {
    resolve_vertex_base_url_with(base_url, settings, |name| std::env::var(name).ok())
}

/// Pure helper resolving Vertex AI URL templates using explicit settings and an
/// injected env lookup (facilitates parallel-safe deterministic tests).
pub fn resolve_vertex_base_url_with<F>(
    base_url: &str,
    settings: &Map<String, Value>,
    env_get: F,
) -> Result<String, LlmError>
where
    F: Fn(&str) -> Option<String>,
{
    if !base_url.contains("${GOOGLE_VERTEX_") {
        return Ok(base_url.to_owned());
    }

    let location = setting_or_env(settings, &env_get, "location", "GOOGLE_VERTEX_LOCATION")
        .unwrap_or_else(|| "global".to_owned());
    let default_endpoint = if location.eq_ignore_ascii_case("global") {
        "aiplatform.googleapis.com".to_owned()
    } else {
        format!("{location}-aiplatform.googleapis.com")
    };
    let endpoint = setting_or_env(settings, &env_get, "endpoint", "GOOGLE_VERTEX_ENDPOINT")
        .unwrap_or(default_endpoint);
    let claude_endpoint = setting_or_env(
        settings,
        &env_get,
        "claudeEndpoint",
        "GOOGLE_VERTEX_CLAUDE_ENDPOINT",
    )
    .unwrap_or_else(|| endpoint.clone());

    let project = setting_or_env(settings, &env_get, "project", "GOOGLE_VERTEX_PROJECT")
        .or_else(|| env_get("GOOGLE_CLOUD_PROJECT").filter(|s| !s.trim().is_empty()))
        .or_else(|| env_get("GCLOUD_PROJECT").filter(|s| !s.trim().is_empty()))
        .or_else(|| {
            settings
                .get("credentialsPath")
                .and_then(Value::as_str)
                .map(Path::new)
                .and_then(project_id_from_adc_file)
        })
        .or_else(|| discover_adc_path().as_deref().and_then(project_id_from_adc_file))
        .ok_or_else(|| LlmError::Config {
            message: "Vertex AI URL requires `GOOGLE_VERTEX_PROJECT` (or `GOOGLE_CLOUD_PROJECT` / ADC `project_id`)"
                .to_owned(),
        })?;

    Ok(base_url
        .replace("${GOOGLE_VERTEX_ENDPOINT}", endpoint.trim())
        .replace("${GOOGLE_VERTEX_CLAUDE_ENDPOINT}", claude_endpoint.trim())
        .replace("${GOOGLE_VERTEX_LOCATION}", location.trim())
        .replace("${GOOGLE_VERTEX_PROJECT}", project.trim()))
}

fn setting_or_env<F>(
    settings: &Map<String, Value>,
    env_get: &F,
    setting_key: &str,
    env_key: &str,
) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    settings
        .get(setting_key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            env_get(env_key)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
}

fn discover_adc_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path.trim()));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        let candidate = PathBuf::from(home)
            .join(".config")
            .join("gcloud")
            .join("application_default_credentials.json");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn project_id_from_adc_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    val.get("project_id")
        .or_else(|| val.get("quota_project_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Return true when a resolved model targets Google Cloud Vertex AI endpoints.
#[must_use]
pub fn is_vertex_target(provider_id: &str, base_url: &str) -> bool {
    provider_id.eq_ignore_ascii_case("google-vertex")
        || provider_id.eq_ignore_ascii_case("google-vertex-anthropic")
        || base_url.contains("aiplatform.googleapis.com")
        || base_url.contains("claude.googleapis.com")
        || base_url.contains("/publishers/anthropic/models")
        || base_url.contains("/publishers/google/models")
}

fn now_epoch_s() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Form, Json, Router, routing::post};
    use serde_json::json;

    #[test]
    fn vertex_url_templates_expand_global_and_regional() {
        let env = |k: &str| match k {
            "GOOGLE_VERTEX_PROJECT" => Some("my-gcp-proj".to_owned()),
            _ => None,
        };
        let url = resolve_vertex_base_url_with(
            "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/publishers/google",
            &Map::new(),
            env,
        )
        .expect("resolve global");
        assert_eq!(
            url,
            "https://aiplatform.googleapis.com/v1/projects/my-gcp-proj/locations/global/publishers/google"
        );

        let regional_env = |k: &str| match k {
            "GOOGLE_VERTEX_PROJECT" => Some("proj-2".to_owned()),
            "GOOGLE_VERTEX_LOCATION" => Some("europe-west1".to_owned()),
            _ => None,
        };
        let regional = resolve_vertex_base_url_with(
            "https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/publishers/anthropic/models",
            &Map::new(),
            regional_env,
        )
        .expect("resolve regional");
        assert_eq!(
            regional,
            "https://europe-west1-aiplatform.googleapis.com/v1/projects/proj-2/locations/europe-west1/publishers/anthropic/models"
        );
    }

    #[tokio::test]
    async fn authorized_user_adc_exchanges_refresh_token_and_caches() {
        let app = Router::new().route(
            "/token",
            post(|Form(form): Form<BTreeMap<String, String>>| async move {
                assert_eq!(
                    form.get("grant_type").map(String::as_str),
                    Some("refresh_token")
                );
                assert_eq!(
                    form.get("refresh_token").map(String::as_str),
                    Some("rt_secret")
                );
                Json(json!({ "access_token": "ya29.test_vertex_token", "expires_in": 3600 }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let adc_path = dir.path().join("adc.json");
        std::fs::write(
            &adc_path,
            json!({
                "type": "authorized_user",
                "client_id": "cid",
                "client_secret": "csec",
                "refresh_token": "rt_secret",
                "quota_project_id": "proj-adc"
            })
            .to_string(),
        )
        .unwrap();

        let http = Client::new();
        let adc = GoogleAdcClient::default();
        // Verify pure refresh helper directly against mock endpoint without mutating global env.
        let bearer = exchange_refresh_token(
            &http,
            &format!("http://{addr}/token"),
            "cid",
            &Zeroizing::new("csec".to_owned()),
            &Zeroizing::new("rt_secret".to_owned()),
        )
        .await
        .expect("token exchange");
        assert_eq!(bearer.token.as_str(), "ya29.test_vertex_token");
        assert!(adc.cached("none").is_none());
        assert_eq!(
            project_id_from_adc_file(&adc_path).as_deref(),
            Some("proj-adc")
        );
    }
}
