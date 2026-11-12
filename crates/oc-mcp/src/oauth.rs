//! OAuth 2.1 support for remote MCP servers.
//!
//! Implements protected-resource/authorization-server metadata discovery,
//! PKCE (S256) authorization URLs, code exchange, and refresh. Tokens are
//! persisted through an injected [`CredentialStore`]; this crate never touches
//! a database. The interactive browser/loopback callback is deliberately out
//! of scope: [`OAuthSession::begin`] returns the URL the host opens, and
//! [`OAuthSession::complete`] accepts the returned code.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::OAuthConfig;
use crate::error::{McpError, McpResult};

/// Discovery and token request timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Refresh this long before access-token expiry.
pub const REFRESH_SKEW_MS: i64 = 60_000;

/// RFC 8414 authorization-server metadata (also used for OIDC discovery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

/// RFC 9728 protected-resource metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResourceMetadata {
    pub resource: String,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
}

/// Result of metadata discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub resource: ProtectedResourceMetadata,
    pub authorization_server: OAuthMetadata,
}

/// OAuth token response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_owned()
}

/// Credentials persisted per server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
    /// Epoch milliseconds; absent means non-expiring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

/// Injected token persistence (the server wires this to the credential table).
#[async_trait]
pub trait CredentialStore: Send + Sync + std::fmt::Debug {
    /// Loads credentials for a server.
    async fn load(&self, server: &str) -> McpResult<Option<StoredCredentials>>;
    /// Saves credentials for a server.
    async fn save(&self, server: &str, credentials: &StoredCredentials) -> McpResult<()>;
    /// Removes credentials for a server.
    async fn remove(&self, server: &str) -> McpResult<()>;
}

/// In-memory credential store for tests and single-process use.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    entries: Mutex<HashMap<String, StoredCredentials>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl MemoryCredentialStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn load(&self, server: &str) -> McpResult<Option<StoredCredentials>> {
        Ok(lock(&self.entries).get(server).cloned())
    }

    async fn save(&self, server: &str, credentials: &StoredCredentials) -> McpResult<()> {
        lock(&self.entries).insert(server.to_owned(), credentials.clone());
        Ok(())
    }

    async fn remove(&self, server: &str) -> McpResult<()> {
        lock(&self.entries).remove(server);
        Ok(())
    }
}

/// PKCE verifier/challenge pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// A fresh PKCE pair using the S256 method.
#[must_use]
pub fn pkce_pair() -> Pkce {
    let verifier = generate_code_verifier();
    let challenge = code_challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// Generates a 43-character base64url verifier from 32 random bytes.
#[must_use]
pub fn generate_code_verifier() -> String {
    let bytes: [u8; 32] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// S256 challenge for a verifier (SHA-256, base64url without padding).
#[must_use]
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Random OAuth `state` value.
#[must_use]
pub fn random_state() -> String {
    let bytes: [u8; 32] = rand::random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Parameters for building an authorization URL.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub state: &'a str,
    pub code_challenge: &'a str,
    pub scope: Option<&'a str>,
    pub resource: Option<&'a str>,
}

/// Builds the authorization-code URL with PKCE and an RFC 8707 resource.
pub fn authorization_url(
    metadata: &OAuthMetadata,
    request: &AuthorizationRequest<'_>,
) -> McpResult<Url> {
    let mut url = Url::parse(&metadata.authorization_endpoint)
        .map_err(|error| McpError::OAuth(format!("invalid authorization_endpoint: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", request.client_id)
            .append_pair("redirect_uri", request.redirect_uri)
            .append_pair("state", request.state)
            .append_pair("code_challenge", request.code_challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(scope) = request.scope {
            query.append_pair("scope", scope);
        }
        if let Some(resource) = request.resource {
            query.append_pair("resource", resource);
        }
    }
    Ok(url)
}

fn http_client() -> McpResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("opencode/{}", env!("CARGO_PKG_VERSION")))
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| McpError::OAuth(error.to_string()))
}

async fn fetch_json<T: DeserializeOwned>(http: &reqwest::Client, url: &Url) -> McpResult<T> {
    let response = http
        .get(url.clone())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| McpError::Network(error.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| McpError::Network(error.to_string()))?;
    if !status.is_success() {
        return Err(McpError::OAuth(format!(
            "HTTP {status} fetching {url}: {}",
            body.trim()
        )));
    }
    serde_json::from_str(&body)
        .map_err(|error| McpError::OAuth(format!("invalid metadata at {url}: {error}")))
}

fn well_known(base: &Url, suffix: &str) -> Url {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    let target = if path.is_empty() {
        suffix.to_owned()
    } else {
        format!("{suffix}{path}")
    };
    url.set_path(&target);
    url
}

/// Discovers protected-resource and authorization-server metadata.
pub async fn discover(
    http: &reqwest::Client,
    server_url: &Url,
    config: &OAuthConfig,
) -> McpResult<Discovery> {
    if let Some(metadata_url) = config.auth_server_metadata_url.as_deref() {
        let url = Url::parse(metadata_url).map_err(|error| {
            McpError::OAuth(format!("invalid auth_server_metadata_url: {error}"))
        })?;
        let metadata: OAuthMetadata = fetch_json(http, &url).await?;
        return Ok(Discovery {
            resource: ProtectedResourceMetadata {
                resource: resource_identity(server_url),
                authorization_servers: vec![metadata.issuer.clone()],
                scopes_supported: metadata.scopes_supported.clone(),
            },
            authorization_server: metadata,
        });
    }

    let resource = match fetch_protected_resource(http, server_url).await {
        Ok(resource) => resource,
        Err(error) => {
            tracing::debug!(%error, "protected resource metadata unavailable; probing authorization server directly");
            let metadata = fetch_authorization_metadata(http, server_url).await?;
            return Ok(Discovery {
                resource: ProtectedResourceMetadata {
                    resource: resource_identity(server_url),
                    authorization_servers: vec![metadata.issuer.clone()],
                    scopes_supported: metadata.scopes_supported.clone(),
                },
                authorization_server: metadata,
            });
        }
    };
    let issuer = resource.authorization_servers.first().ok_or_else(|| {
        McpError::OAuth("protected resource metadata names no authorization server".to_owned())
    })?;
    let issuer_url = Url::parse(issuer)
        .map_err(|error| McpError::OAuth(format!("invalid authorization server URL: {error}")))?;
    let authorization_server = fetch_authorization_metadata(http, &issuer_url).await?;
    Ok(Discovery {
        resource,
        authorization_server,
    })
}

async fn fetch_protected_resource(
    http: &reqwest::Client,
    server_url: &Url,
) -> McpResult<ProtectedResourceMetadata> {
    let mut candidates = vec![
        well_known(server_url, "/.well-known/oauth-protected-resource"),
        well_known(server_url, "/.well-known/oauth-protected-resource/"),
    ];
    candidates.dedup();
    let mut last = None;
    for candidate in candidates {
        match fetch_json(http, &candidate).await {
            Ok(metadata) => return Ok(metadata),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| McpError::OAuth("no protected resource metadata".to_owned())))
}

async fn fetch_authorization_metadata(
    http: &reqwest::Client,
    issuer: &Url,
) -> McpResult<OAuthMetadata> {
    let mut candidates = vec![
        well_known(issuer, "/.well-known/oauth-authorization-server"),
        well_known(issuer, "/.well-known/openid-configuration"),
    ];
    // OIDC requires the suffix form; RFC 8414 permits insertion before the path.
    let mut root = issuer.clone();
    root.set_query(None);
    root.set_fragment(None);
    root.set_path("/.well-known/oauth-authorization-server");
    candidates.push(root.clone());
    root.set_path("/.well-known/openid-configuration");
    candidates.push(root);
    candidates.dedup();
    let mut last = None;
    for candidate in candidates {
        match fetch_json::<OAuthMetadata>(http, &candidate).await {
            Ok(metadata) => return Ok(metadata),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| McpError::OAuth("no authorization server metadata".to_owned())))
}

fn resource_identity(server_url: &Url) -> String {
    let mut url = server_url.clone();
    url.set_fragment(None);
    url.to_string()
}

/// Parameters for exchanging an authorization code.
#[derive(Debug, Clone)]
pub struct CodeExchange<'a> {
    pub token_endpoint: &'a str,
    pub client_id: &'a str,
    pub client_secret: Option<&'a str>,
    pub code: &'a str,
    pub code_verifier: &'a str,
    pub redirect_uri: &'a str,
    pub resource: Option<&'a str>,
}

/// Exchanges an authorization code for tokens.
pub async fn exchange_code(
    http: &reqwest::Client,
    exchange: &CodeExchange<'_>,
) -> McpResult<OAuthTokens> {
    let mut form = vec![
        ("grant_type".to_owned(), "authorization_code".to_owned()),
        ("code".to_owned(), exchange.code.to_owned()),
        ("client_id".to_owned(), exchange.client_id.to_owned()),
        ("redirect_uri".to_owned(), exchange.redirect_uri.to_owned()),
        (
            "code_verifier".to_owned(),
            exchange.code_verifier.to_owned(),
        ),
    ];
    if let Some(secret) = exchange.client_secret {
        form.push(("client_secret".to_owned(), secret.to_owned()));
    }
    if let Some(resource) = exchange.resource {
        form.push(("resource".to_owned(), resource.to_owned()));
    }
    token_request(http, exchange.token_endpoint, &form).await
}

/// Parameters for refreshing an access token.
#[derive(Debug, Clone)]
pub struct Refresh<'a> {
    pub token_endpoint: &'a str,
    pub client_id: &'a str,
    pub client_secret: Option<&'a str>,
    pub refresh_token: &'a str,
    pub resource: Option<&'a str>,
}

/// Refreshes an access token.
pub async fn refresh(http: &reqwest::Client, refresh: &Refresh<'_>) -> McpResult<OAuthTokens> {
    let mut form = vec![
        ("grant_type".to_owned(), "refresh_token".to_owned()),
        ("refresh_token".to_owned(), refresh.refresh_token.to_owned()),
        ("client_id".to_owned(), refresh.client_id.to_owned()),
    ];
    if let Some(secret) = refresh.client_secret {
        form.push(("client_secret".to_owned(), secret.to_owned()));
    }
    if let Some(resource) = refresh.resource {
        form.push(("resource".to_owned(), resource.to_owned()));
    }
    token_request(http, refresh.token_endpoint, &form).await
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(String, String)],
) -> McpResult<OAuthTokens> {
    let url = Url::parse(endpoint)
        .map_err(|error| McpError::OAuth(format!("invalid token_endpoint: {error}")))?;
    let response = http
        .post(url)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await
        .map_err(|error| McpError::Network(error.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| McpError::Network(error.to_string()))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error_description")
                    .or_else(|| value.get("error"))
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| body.trim().to_owned());
        return Err(McpError::OAuth(format!(
            "token endpoint returned HTTP {status}: {detail}"
        )));
    }
    let tokens: OAuthTokens = serde_json::from_str(&body)
        .map_err(|error| McpError::OAuth(format!("invalid token response: {error}")))?;
    if tokens.access_token.is_empty() {
        return Err(McpError::OAuth(
            "token response has an empty access token".to_owned(),
        ));
    }
    Ok(tokens)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Longest token lifetime accepted from a token endpoint: 366 days.
const MAX_TOKEN_LIFETIME_SECONDS: u64 = 366 * 24 * 60 * 60;

/// Converts a token endpoint's `expires_in` into an absolute epoch-millisecond
/// deadline.
///
/// A hostile `expires_in` must not overflow the seconds-to-milliseconds
/// conversion: `try_from` alone rejects only values above `i64::MAX`, so
/// anything above `i64::MAX / 1_000` panicked in debug and wrapped to an
/// already-expired instant in release. The lifetime is clamped to
/// [`MAX_TOKEN_LIFETIME_SECONDS`] and the arithmetic saturates, so the result is
/// always a future deadline.
fn expiry_deadline(expires_in: Option<u64>) -> Option<i64> {
    let seconds = expires_in?.min(MAX_TOKEN_LIFETIME_SECONDS);
    let millis = i64::try_from(seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000);
    Some(now_ms().saturating_add(millis))
}

/// Result of beginning an interactive authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub url: Url,
    pub state: String,
}

/// OAuth client state for one MCP server.
#[derive(Debug)]
pub struct OAuthSession {
    server: String,
    url: Url,
    config: OAuthConfig,
    store: Arc<dyn CredentialStore>,
    http: reqwest::Client,
}

impl OAuthSession {
    /// Creates a session with a default HTTP client.
    pub fn new(
        server: impl Into<String>,
        url: Url,
        config: OAuthConfig,
        store: Arc<dyn CredentialStore>,
    ) -> McpResult<Self> {
        Ok(Self {
            server: server.into(),
            url,
            config,
            store,
            http: http_client()?,
        })
    }

    /// Uses a caller-provided HTTP client (tests, shared agents).
    #[must_use]
    pub fn with_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Discovers metadata for this server.
    pub async fn discover(&self) -> McpResult<Discovery> {
        discover(&self.http, &self.url, &self.config).await
    }

    fn client_id(&self) -> McpResult<&str> {
        self.config.client_id.as_deref().ok_or_else(|| {
            McpError::OAuth(format!(
                "MCP server \"{}\" has no oauth.client_id; dynamic client registration is not implemented",
                self.server
            ))
        })
    }

    /// Discovers metadata, stores a PKCE verifier and returns the URL the host
    /// must open in a browser.
    pub async fn begin(&self, redirect_uri: impl Into<String>) -> McpResult<Authorization> {
        let client_id = self.client_id()?.to_owned();
        let discovery = self.discover().await?;
        let redirect_uri = redirect_uri.into();
        let pkce = pkce_pair();
        let state = random_state();
        let request = AuthorizationRequest {
            client_id: &client_id,
            redirect_uri: &redirect_uri,
            state: &state,
            code_challenge: &pkce.challenge,
            scope: self.config.scope.as_deref(),
            resource: Some(&discovery.resource.resource),
        };
        let url = authorization_url(&discovery.authorization_server, &request)?;
        self.store
            .save(
                &self.server,
                &StoredCredentials {
                    code_verifier: Some(pkce.verifier),
                    state: Some(state.clone()),
                    redirect_uri: Some(redirect_uri),
                    token_endpoint: Some(discovery.authorization_server.token_endpoint.clone()),
                    ..StoredCredentials::default()
                },
            )
            .await?;
        Ok(Authorization { url, state })
    }

    /// Exchanges the authorization code returned by the host's callback.
    pub async fn complete(&self, code: &str) -> McpResult<OAuthTokens> {
        let client_id = self.client_id()?.to_owned();
        let stored = self.store.load(&self.server).await?.ok_or_else(|| {
            McpError::OAuth("authorization was not started for this server".to_owned())
        })?;
        let verifier = stored.code_verifier.clone().ok_or_else(|| {
            McpError::OAuth("missing PKCE code verifier for MCP OAuth flow".to_owned())
        })?;
        let redirect_uri = stored
            .redirect_uri
            .clone()
            .or_else(|| self.config.redirect_uri.clone())
            .ok_or_else(|| McpError::OAuth("missing redirect URI for MCP OAuth flow".to_owned()))?;
        let token_endpoint = match stored.token_endpoint.clone() {
            Some(endpoint) => endpoint,
            None => self.discover().await?.authorization_server.token_endpoint,
        };
        let tokens = exchange_code(
            &self.http,
            &CodeExchange {
                token_endpoint: &token_endpoint,
                client_id: &client_id,
                client_secret: self.config.client_secret.as_deref(),
                code,
                code_verifier: &verifier,
                redirect_uri: &redirect_uri,
                resource: Some(&resource_identity(&self.url)),
            },
        )
        .await?;
        let expires_at = expiry_deadline(tokens.expires_in);
        self.store
            .save(
                &self.server,
                &StoredCredentials {
                    tokens: Some(tokens.clone()),
                    expires_at,
                    token_endpoint: Some(token_endpoint),
                    redirect_uri: Some(redirect_uri),
                    ..StoredCredentials::default()
                },
            )
            .await?;
        Ok(tokens)
    }

    /// Returns a usable bearer token, refreshing shortly before expiry.
    ///
    /// A failed refresh becomes [`McpError::NeedsAuth`]; it never retries in a
    /// loop.
    pub async fn access_token(&self) -> McpResult<String> {
        let stored = self
            .store
            .load(&self.server)
            .await?
            .ok_or_else(|| McpError::NeedsAuth {
                server: self.server.clone(),
                message: "no stored credentials".to_owned(),
            })?;
        let Some(tokens) = stored.tokens.clone() else {
            return Err(McpError::NeedsAuth {
                server: self.server.clone(),
                message: "authorization is required".to_owned(),
            });
        };
        let expiring = stored
            .expires_at
            .is_some_and(|expires| now_ms() + REFRESH_SKEW_MS >= expires);
        if !expiring {
            return Ok(tokens.access_token);
        }
        let client_id = self.client_id()?.to_owned();
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| McpError::NeedsAuth {
                server: self.server.clone(),
                message: "access token expired and no refresh token is stored".to_owned(),
            })?;
        let token_endpoint = match stored.token_endpoint.clone() {
            Some(endpoint) => endpoint,
            None => self.discover().await?.authorization_server.token_endpoint,
        };
        let refreshed = refresh(
            &self.http,
            &Refresh {
                token_endpoint: &token_endpoint,
                client_id: &client_id,
                client_secret: self.config.client_secret.as_deref(),
                refresh_token: &refresh_token,
                resource: Some(&resource_identity(&self.url)),
            },
        )
        .await
        .map_err(|error| McpError::NeedsAuth {
            server: self.server.clone(),
            message: format!("MCP authorization expired: {error}"),
        })?;
        let expires_at = expiry_deadline(refreshed.expires_in);
        let merged = OAuthTokens {
            refresh_token: refreshed
                .refresh_token
                .clone()
                .or_else(|| tokens.refresh_token.clone()),
            ..refreshed.clone()
        };
        self.store
            .save(
                &self.server,
                &StoredCredentials {
                    tokens: Some(merged),
                    expires_at,
                    token_endpoint: Some(token_endpoint),
                    redirect_uri: stored.redirect_uri,
                    ..StoredCredentials::default()
                },
            )
            .await?;
        Ok(refreshed.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_url_safe_and_long_enough() {
        let pkce = pkce_pair();
        assert!((43..=128).contains(&pkce.verifier.len()));
        assert!(
            pkce.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(pkce.challenge, code_challenge(&pkce.verifier));
        assert_ne!(random_state(), random_state());
    }

    #[test]
    fn authorization_url_contains_pkce_and_resource() {
        let metadata = OAuthMetadata {
            issuer: "https://auth.example.com".to_owned(),
            authorization_endpoint: "https://auth.example.com/authorize".to_owned(),
            token_endpoint: "https://auth.example.com/token".to_owned(),
            registration_endpoint: None,
            scopes_supported: None,
            code_challenge_methods_supported: Some(vec!["S256".to_owned()]),
            token_endpoint_auth_methods_supported: None,
        };
        let url = authorization_url(
            &metadata,
            &AuthorizationRequest {
                client_id: "client",
                redirect_uri: "http://127.0.0.1:1234/callback",
                state: "state",
                code_challenge: "challenge",
                scope: Some("repo read"),
                resource: Some("https://mcp.example.com/mcp"),
            },
        )
        .expect("url");
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["client_id"], "client");
        assert_eq!(pairs["scope"], "repo read");
        assert_eq!(pairs["resource"], "https://mcp.example.com/mcp");
    }

    #[test]
    fn well_known_keeps_resource_path() {
        let url = Url::parse("https://example.com/api/mcp").expect("url");
        assert_eq!(
            well_known(&url, "/.well-known/oauth-protected-resource").as_str(),
            "https://example.com/.well-known/oauth-protected-resource/api/mcp"
        );
    }
}
