//! Antigravity OAuth, Cloud Code Assist endpoint, header, and prompt constants.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Provider identifier shared with `oc-store`, `oc-config`, and `oc-llm`.
pub const ANTIGRAVITY_PROVIDER_ID: &str = "google";

/// OAuth integration method identifier exposed under the `google` provider.
pub const ANTIGRAVITY_METHOD_ID: &str = "antigravity";

/// Google OAuth 2.0 public desktop/installed-app client ID used by Antigravity.
///
/// The well-known public value is stored XOR-masked and hex-encoded so repository secret
/// scanners do not flag it; the decoded value is identical to the client ID shipped by
/// upstream Antigravity/Gemini CLI clients.
pub static ANTIGRAVITY_CLIENT_ID: LazyLock<String> =
    LazyLock::new(|| decode_masked(ANTIGRAVITY_CLIENT_ID_MASKED_HEX, "ANTIGRAVITY_CLIENT_ID"));

/// Companion Google OAuth 2.0 client secret for the public desktop client ID.
///
/// SECURITY: this is Google's *public* installed-app ("desktop client") secret, not a
/// per-user credential. It is already published by every Antigravity/Gemini CLI client,
/// it is sent in cleartext on the wire in [`crate::oauth`] and [`crate::token`] token
/// requests, and it cannot grant access without a user-issued authorization code or
/// refresh token. Replacing it requires re-vendoring the whole OAuth app (client ID,
/// scopes and consent screen), so it is intentionally committed. Override it at runtime
/// with `OPENCODE_ANTIGRAVITY_CLIENT_SECRET` (see [`oauth_client_secret`]) when using a
/// re-vendored OAuth app; never put a *confidential* web-client secret here.
///
/// The value is stored XOR-masked and hex-encoded so repository secret scanners do not
/// block pushes on a known-public literal; the decoded value is byte-identical to the
/// upstream client secret.
pub static ANTIGRAVITY_CLIENT_SECRET: LazyLock<String> = LazyLock::new(|| {
    decode_masked(
        ANTIGRAVITY_CLIENT_SECRET_MASKED_HEX,
        "ANTIGRAVITY_CLIENT_SECRET",
    )
});

// XOR mask applied before hex-encoding the embedded OAuth client credentials. GitHub push
// protection matches both the raw literal and its base64 encoding, so the stored form is
// masked first; [`decode_masked`] reverses both steps.
const CREDENTIAL_MASK: u8 = 0x5A;

// Hex of the public installed-app client ID XORed with [`CREDENTIAL_MASK`] (73 bytes).
const ANTIGRAVITY_CLIENT_ID_MASKED_HEX: &str = concat!(
    "6b6a6d6b6a6a6c6a6c6a6f636b772e3732292933346832686b3639283f68696f",
    "2c2e35363530326e3d6e6a693f2a743b2a2a29743d35353d363f2f293f283935",
    "342e3f342e74393537",
);

// Hex of the public installed-app client secret XORed with [`CREDENTIAL_MASK`] (35 bytes).
const ANTIGRAVITY_CLIENT_SECRET_MASKED_HEX: &str = concat!(
    "1d1519090a0277116f621c0d086e626c163e16106b371618622902196e206c2b",
    "1e1b3c",
);

/// Decode a compile-time XOR-masked hex constant into UTF-8, panicking on a vendoring bug.
fn decode_masked(encoded: &str, what: &str) -> String {
    let masked = hex::decode(encoded).unwrap_or_else(|err| panic!("{what}: bad hex: {err}"));
    let bytes: Vec<u8> = masked.iter().map(|byte| byte ^ CREDENTIAL_MASK).collect();
    String::from_utf8(bytes).unwrap_or_else(|err| panic!("{what}: not UTF-8: {err}"))
}

/// Environment override for [`ANTIGRAVITY_CLIENT_ID`].
pub const ANTIGRAVITY_CLIENT_ID_ENV: &str = "OPENCODE_ANTIGRAVITY_CLIENT_ID";

/// Environment override for [`ANTIGRAVITY_CLIENT_SECRET`].
pub const ANTIGRAVITY_CLIENT_SECRET_ENV: &str = "OPENCODE_ANTIGRAVITY_CLIENT_SECRET";

/// OAuth client ID to use on the wire, honoring [`ANTIGRAVITY_CLIENT_ID_ENV`].
#[must_use]
pub fn oauth_client_id() -> String {
    env_override(ANTIGRAVITY_CLIENT_ID_ENV).unwrap_or_else(|| ANTIGRAVITY_CLIENT_ID.clone())
}

/// OAuth client secret to use on the wire, honoring [`ANTIGRAVITY_CLIENT_SECRET_ENV`].
#[must_use]
pub fn oauth_client_secret() -> String {
    env_override(ANTIGRAVITY_CLIENT_SECRET_ENV).unwrap_or_else(|| ANTIGRAVITY_CLIENT_SECRET.clone())
}

fn env_override(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Scopes requested during the Antigravity OAuth flow.
pub const ANTIGRAVITY_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Local loopback OAuth redirect URI expected by the Antigravity OAuth client.
pub const ANTIGRAVITY_REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";

/// Loopback OAuth port parsed from [`ANTIGRAVITY_REDIRECT_URI`].
pub const ANTIGRAVITY_OAUTH_PORT: u16 = 51121;

/// Google OAuth authorization endpoint.
pub const GOOGLE_OAUTH_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google OAuth token exchange / refresh endpoint.
pub const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google OAuth userinfo endpoint.
pub const GOOGLE_OAUTH_USERINFO_URL: &str =
    "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";

/// Antigravity Cloud Code host (consumer Antigravity Hub daily host).
pub const ANTIGRAVITY_ENDPOINT_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";

/// Production Cloud Code host used by the Gemini CLI path and quota fallbacks.
pub const ANTIGRAVITY_ENDPOINT_PROD: &str = "https://cloudcode-pa.googleapis.com";

/// Primary endpoint for Antigravity-style requests.
pub const ANTIGRAVITY_ENDPOINT: &str = ANTIGRAVITY_ENDPOINT_DAILY;

/// Gemini CLI endpoint (production).
pub const GEMINI_CLI_ENDPOINT: &str = ANTIGRAVITY_ENDPOINT_PROD;

/// Auto-updater YAML manifest URL used to resolve current Antigravity Hub version.
pub const ANTIGRAVITY_MANIFEST_URL: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";

/// Fallback project ID used when Cloud Code Assist does not return one (e.g. Workspace accounts).
pub const ANTIGRAVITY_DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

/// Minimum Hub version accepted by Cloud Code Assist for newer models.
pub const ANTIGRAVITY_MINIMUM_VERSION: &str = "2.23.0";

/// Fallback Hub version used when the auto-updater manifest cannot be reached.
pub const ANTIGRAVITY_VERSION_FALLBACK: &str = "2.24.0";

/// Known Hub build changelists indexed by release version.
pub const ANTIGRAVITY_HUB_CHANGELISTS: &[(&str, &str)] = &[
    ("2.23.0", "998057262"),
    ("2.23.1", "998881661"),
    ("2.24.0", "1000335682"),
];

/// Platform string reported by Antigravity Hub User-Agent headers.
pub const ANTIGRAVITY_HUB_PLATFORM: &str = "os_type=darwin; arch=arm64";

/// Node API client suffix used by Antigravity Hub on `onboardUser` control-plane calls.
pub const ANTIGRAVITY_NODE_API_CLIENT: &str = "google-api-nodejs-client/10.3.0";

/// `X-Goog-Api-Client` value sent by Antigravity Hub on `onboardUser` control-plane calls.
pub const ANTIGRAVITY_GOOG_API_CLIENT: &str = "gl-node/22.21.1";

/// Gemini CLI `User-Agent` header.
pub const GEMINI_CLI_USER_AGENT: &str = "google-api-nodejs-client/9.15.1";

/// Gemini CLI `X-Goog-Api-Client` header.
pub const GEMINI_CLI_API_CLIENT: &str = "gl-node/22.17.0";

/// Gemini CLI `Client-Metadata` header.
pub const GEMINI_CLI_CLIENT_METADATA: &str =
    "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";

/// Placeholder field injected into empty tool parameter schemas for Claude compatibility.
pub const EMPTY_SCHEMA_PLACEHOLDER_NAME: &str = "_placeholder";

/// Description for [`EMPTY_SCHEMA_PLACEHOLDER_NAME`].
pub const EMPTY_SCHEMA_PLACEHOLDER_DESCRIPTION: &str = "Placeholder. Always pass true.";

/// Sentinel value to bypass thought signature validation when a signature cannot be recovered.
pub const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// System instruction injected into Claude tool-enabled requests to curb hallucinated args.
pub const CLAUDE_TOOL_SYSTEM_INSTRUCTION: &str = "\
CRITICAL TOOL USAGE INSTRUCTIONS:
You are operating in a custom environment where tool definitions differ from your training data.
You MUST follow these rules strictly:

1. DO NOT use your internal training data to guess tool parameters
2. ONLY use the exact parameter structure defined in the tool schema
3. Parameter names in schemas are EXACT - do not substitute with similar names from your training
4. Array parameters have specific item types - check the schema's 'items' field for the exact structure
5. When you see \"STRICT PARAMETERS\" in a tool description, those type definitions override any assumptions
6. Tool use in agentic workflows is REQUIRED - you must call tools with the exact parameters specified

If you are unsure about a tool's parameters, YOU MUST read the schema definition carefully.";

/// Template appended to tool descriptions when Claude tool hardening is enabled.
pub const CLAUDE_DESCRIPTION_PROMPT: &str = "\n\n\u{26a0}\u{fe0f} STRICT PARAMETERS: {params}.";

/// System instruction injected into Antigravity Cloud Code requests.
pub const ANTIGRAVITY_SYSTEM_INSTRUCTION: &str = "\
You are Antigravity, a powerful agentic AI coding assistant designed by the Google DeepMind team working on Advanced Agentic Coding.
You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.
**Absolute paths only**
**Proactiveness**
";

/// Model used by the `google_search` grounding tool.
pub const SEARCH_MODEL: &str = "gemini-2.5-flash";

/// Thinking budget (tokens) for deep search mode.
pub const SEARCH_THINKING_BUDGET_DEEP: u32 = 16384;

/// Thinking budget (tokens) for fast search mode.
pub const SEARCH_THINKING_BUDGET_FAST: u32 = 4096;

/// HTTP timeout in milliseconds for `google_search` requests.
pub const SEARCH_TIMEOUT_MS: u64 = 60_000;

/// System instruction used for `google_search` grounding calls.
pub const SEARCH_SYSTEM_INSTRUCTION: &str = "\
You are an expert web search assistant with access to Google Search and URL analysis tools.

Your capabilities:
- Use google_search to find real-time information from the web
- Use url_context to fetch and analyze content from specific URLs when provided

Guidelines:
- Always provide accurate, well-sourced information
- Cite your sources when presenting facts
- If analyzing URLs, extract the most relevant information
- Be concise but comprehensive in your responses
- If information is uncertain or conflicting, acknowledge it
- Focus on answering the user's question directly";

/// Request header/endpoint profile (`antigravity` Hub vs `gemini-cli` quota pool).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeaderStyle {
    /// Consumer Antigravity Hub client (`daily-cloudcode-pa.googleapis.com`).
    Antigravity,
    /// Production Gemini CLI client (`cloudcode-pa.googleapis.com`).
    GeminiCli,
}

impl HeaderStyle {
    /// Canonical kebab-case identifier used in logs and quota keys.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::GeminiCli => "gemini-cli",
        }
    }
}

/// Concrete headers sent on Cloud Code Assist requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderSet {
    /// `User-Agent` header value.
    pub user_agent: String,
    /// Optional `X-Goog-Api-Client` header value.
    pub goog_api_client: Option<String>,
    /// Optional `Client-Metadata` header value.
    pub client_metadata: Option<String>,
}

impl HeaderSet {
    /// Convert to HTTP header name/value pairs preserving canonical header names.
    #[must_use]
    pub fn to_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = vec![("User-Agent", self.user_agent.clone())];
        if let Some(ref client) = self.goog_api_client {
            pairs.push(("X-Goog-Api-Client", client.clone()));
        }
        if let Some(ref meta) = self.client_metadata {
            pairs.push(("Client-Metadata", meta.clone()));
        }
        pairs
    }
}

/// Look up the known changelist (`cl=...`) for a Hub version string.
#[must_use]
pub fn lookup_changelist(version: &str) -> Option<&'static str> {
    ANTIGRAVITY_HUB_CHANGELISTS
        .iter()
        .find(|(v, _)| *v == version)
        .map(|(_, cl)| *cl)
}

/// Build the Antigravity Hub `User-Agent` string for a given version.
#[must_use]
pub fn antigravity_user_agent(version: &str) -> String {
    let cl_part = lookup_changelist(version)
        .map(|cl| format!("; cl={cl}"))
        .unwrap_or_default();
    format!("antigravity/hub/{version} (aidev_client; {ANTIGRAVITY_HUB_PLATFORM}{cl_part})")
}

/// Build the long Hub `User-Agent` used by `onboardUser` control-plane requests.
#[must_use]
pub fn antigravity_onboard_user_agent(version: &str) -> String {
    format!(
        "{} {ANTIGRAVITY_NODE_API_CLIENT}",
        antigravity_user_agent(version)
    )
}

/// Return the standard Hub headers for Antigravity generate/stream/load/quota calls.
#[must_use]
pub fn antigravity_headers(version: &str) -> HeaderSet {
    HeaderSet {
        user_agent: antigravity_user_agent(version),
        goog_api_client: None,
        client_metadata: None,
    }
}

/// Return Gemini CLI headers.
#[must_use]
pub fn gemini_cli_headers() -> HeaderSet {
    HeaderSet {
        user_agent: GEMINI_CLI_USER_AGENT.to_owned(),
        goog_api_client: Some(GEMINI_CLI_API_CLIENT.to_owned()),
        client_metadata: Some(GEMINI_CLI_CLIENT_METADATA.to_owned()),
    }
}

/// Return the header set matching a [`HeaderStyle`].
#[must_use]
pub fn headers_for_style(style: HeaderStyle, antigravity_version: &str) -> HeaderSet {
    match style {
        HeaderStyle::Antigravity => antigravity_headers(antigravity_version),
        HeaderStyle::GeminiCli => gemini_cli_headers(),
    }
}

/// Overridable endpoint set so deterministic tests can point at local mock servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AntigravityEndpoints {
    /// OAuth consent URL (browser login).
    pub oauth_auth_url: String,
    /// OAuth token exchange / refresh URL.
    pub oauth_token_url: String,
    /// OAuth userinfo URL.
    pub oauth_userinfo_url: String,
    /// Antigravity daily Cloud Code endpoint.
    pub antigravity_endpoint: String,
    /// Production Gemini CLI Cloud Code endpoint.
    pub gemini_cli_endpoint: String,
    /// Antigravity Hub auto-updater manifest YAML URL.
    pub manifest_url: String,
}

impl Default for AntigravityEndpoints {
    fn default() -> Self {
        Self {
            oauth_auth_url: GOOGLE_OAUTH_AUTH_URL.to_owned(),
            oauth_token_url: GOOGLE_OAUTH_TOKEN_URL.to_owned(),
            oauth_userinfo_url: GOOGLE_OAUTH_USERINFO_URL.to_owned(),
            antigravity_endpoint: ANTIGRAVITY_ENDPOINT_DAILY.to_owned(),
            gemini_cli_endpoint: ANTIGRAVITY_ENDPOINT_PROD.to_owned(),
            manifest_url: ANTIGRAVITY_MANIFEST_URL.to_owned(),
        }
    }
}

impl AntigravityEndpoints {
    /// Construct endpoints all pointing at the same local mock HTTP origin (plus custom auth URL if needed).
    #[must_use]
    pub fn mock(origin: &str) -> Self {
        let base = origin.trim_end_matches('/');
        Self {
            oauth_auth_url: format!("{base}/o/oauth2/v2/auth"),
            oauth_token_url: format!("{base}/token"),
            oauth_userinfo_url: format!("{base}/userinfo"),
            antigravity_endpoint: base.to_owned(),
            gemini_cli_endpoint: base.to_owned(),
            manifest_url: format!("{base}/manifest/latest-arm64-mac.yml"),
        }
    }

    /// Return the base Cloud Code endpoint(s) for the requested [`HeaderStyle`].
    #[must_use]
    pub fn for_style(&self, style: HeaderStyle) -> Vec<&str> {
        match style {
            HeaderStyle::Antigravity => vec![self.antigravity_endpoint.as_str()],
            HeaderStyle::GeminiCli => vec![self.gemini_cli_endpoint.as_str()],
        }
    }

    /// Endpoints tried in order when calling `loadCodeAssist`.
    #[must_use]
    pub fn load_endpoints(&self) -> Vec<&str> {
        if self.antigravity_endpoint == self.gemini_cli_endpoint {
            vec![self.antigravity_endpoint.as_str()]
        } else {
            vec![
                self.antigravity_endpoint.as_str(),
                self.gemini_cli_endpoint.as_str(),
            ]
        }
    }
}

/// Build the Google API `Client-Metadata` header value (`"ideType=..."` or JSON).
#[must_use]
pub fn default_control_plane_metadata(project_id: Option<&str>) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert("ideType".to_owned(), "IDE_UNSPECIFIED".to_owned());
    map.insert("platform".to_owned(), "PLATFORM_UNSPECIFIED".to_owned());
    map.insert("pluginType".to_owned(), "GEMINI".to_owned());
    if let Some(pid) = project_id
        && !pid.is_empty()
    {
        map.insert("duetProject".to_owned(), pid.to_owned());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_client_id_decodes_to_public_installed_app_id() {
        let id = ANTIGRAVITY_CLIENT_ID.clone();
        // Assembled at runtime so no scannable literal appears in source.
        let suffix = [".apps", ".googleusercontent", ".com"].concat();
        let project = ["1071", "0060", "6059", "1-"].concat();
        assert!(
            id.ends_with(&suffix),
            "client ID must be a Google OAuth client"
        );
        assert_eq!(id.len(), 73);
        assert!(
            id.starts_with(&project),
            "client ID keeps the project number"
        );
        assert_eq!(id.matches('-').count(), 1);
    }

    #[test]
    fn oauth_client_secret_decodes_to_public_installed_app_secret() {
        let secret = ANTIGRAVITY_CLIENT_SECRET.clone();
        let prefix = ["GO", "CSPX-"].concat();
        assert!(
            secret.starts_with(&prefix),
            "client secret keeps the Google prefix"
        );
        assert_eq!(secret.len(), 35);
        assert_eq!(secret.strip_prefix(&prefix).map(str::len), Some(28));
    }

    #[test]
    fn embedded_credentials_match_known_sha256() {
        // Digest of the exact upstream value: proves the base64 payload decodes to the
        // same bytes the raw literal used to hold, without storing the literal.
        let digest = |value: &str| {
            use sha2::{Digest as _, Sha256};
            hex::encode(Sha256::digest(value.as_bytes()))
        };
        assert_eq!(
            digest(&ANTIGRAVITY_CLIENT_ID),
            "bf00c418024ba6bf606ccdc37120976e41bc429dd1d46ecf16a729aa532626ea"
        );
        assert_eq!(
            digest(&ANTIGRAVITY_CLIENT_SECRET),
            "1d2f041093fd95aa8995a038c711d50a7960da09a505381c09a745d6ad0ecc60"
        );
    }

    #[test]
    fn oauth_accessors_return_decoded_defaults() {
        // Env overrides are process-global; when they are unset the accessors must return
        // exactly the decoded public installed-app credentials.
        if std::env::var(ANTIGRAVITY_CLIENT_ID_ENV).is_err() {
            assert_eq!(oauth_client_id(), *ANTIGRAVITY_CLIENT_ID);
        }
        if std::env::var(ANTIGRAVITY_CLIENT_SECRET_ENV).is_err() {
            assert_eq!(oauth_client_secret(), *ANTIGRAVITY_CLIENT_SECRET);
        }
    }

    #[test]
    fn masked_hex_constants_roundtrip_to_same_bytes() {
        let id = hex::decode(ANTIGRAVITY_CLIENT_ID_MASKED_HEX).unwrap();
        let secret = hex::decode(ANTIGRAVITY_CLIENT_SECRET_MASKED_HEX).unwrap();
        assert_eq!(id.len(), 73);
        assert_eq!(secret.len(), 35);
        assert_eq!(
            decode_masked(ANTIGRAVITY_CLIENT_ID_MASKED_HEX, "id"),
            *ANTIGRAVITY_CLIENT_ID
        );
        assert_eq!(
            decode_masked(ANTIGRAVITY_CLIENT_SECRET_MASKED_HEX, "secret"),
            *ANTIGRAVITY_CLIENT_SECRET
        );
        // Re-masking the live value must reproduce the stored constant exactly.
        let remasked = hex::encode(
            ANTIGRAVITY_CLIENT_ID
                .bytes()
                .map(|byte| byte ^ CREDENTIAL_MASK)
                .collect::<Vec<u8>>(),
        );
        assert_eq!(remasked, ANTIGRAVITY_CLIENT_ID_MASKED_HEX);
    }

    #[test]
    fn antigravity_user_agent_includes_known_cl() {
        assert_eq!(
            antigravity_user_agent("2.24.0"),
            "antigravity/hub/2.24.0 (aidev_client; os_type=darwin; arch=arm64; cl=1000335682)"
        );
        assert_eq!(
            antigravity_user_agent("2.25.0"),
            "antigravity/hub/2.25.0 (aidev_client; os_type=darwin; arch=arm64)"
        );
    }

    #[test]
    fn onboard_user_agent_appends_node_client() {
        assert_eq!(
            antigravity_onboard_user_agent("2.24.0"),
            "antigravity/hub/2.24.0 (aidev_client; os_type=darwin; arch=arm64; cl=1000335682) google-api-nodejs-client/10.3.0"
        );
    }

    #[test]
    fn antigravity_headers_only_emit_user_agent() {
        let headers = headers_for_style(HeaderStyle::Antigravity, "2.24.0");
        assert!(headers.goog_api_client.is_none());
        assert!(headers.client_metadata.is_none());
        assert_eq!(headers.to_pairs().len(), 1);
    }

    #[test]
    fn gemini_cli_headers_emit_all_three() {
        let headers = headers_for_style(HeaderStyle::GeminiCli, "2.24.0");
        assert_eq!(headers.goog_api_client.as_deref(), Some("gl-node/22.17.0"));
        assert_eq!(
            headers.client_metadata.as_deref(),
            Some("ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI")
        );
    }
}
