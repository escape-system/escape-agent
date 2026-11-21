//! Native Antigravity Google OAuth, Cloud Code Assist unified gateway, multi-account
//! pool rotation, model catalog discovery, request/response transforms, and `google_search`
//! grounding tool for `opencode-rust`.

pub mod accounts;
pub mod auth;
pub mod config;
pub mod constants;
pub mod error;
pub mod fingerprint;
pub mod gateway;
pub mod models;
pub mod oauth;
pub mod oauth_server;
pub mod project;
pub mod quota;
pub mod rotation;
pub mod schema;
pub mod search;
pub mod service;
pub mod signature_cache;
pub mod storage;
pub mod thinking;
pub mod token;
pub mod transform;
pub mod version;

pub use accounts::{
    AccountManager, ModelFamily, SelectedAccount, canonical_quota_model_id, quota_key,
    soft_quota_group,
};
pub use auth::{OAuthAuthDetails, RefreshParts, format_refresh_parts, parse_refresh_parts};
pub use config::{
    AccountSelectionStrategy, AntigravityConfig, SchedulingMode, SearchMode, load_config,
    resolve_config_dir,
};
pub use constants::{
    ANTIGRAVITY_CLIENT_ID_ENV, ANTIGRAVITY_CLIENT_SECRET_ENV, ANTIGRAVITY_METHOD_ID,
    ANTIGRAVITY_PROVIDER_ID, AntigravityEndpoints, HeaderStyle, oauth_client_id,
    oauth_client_secret,
};
pub use error::{AntigravityError, Result};
pub use fingerprint::{Fingerprint, FingerprintVersion, generate_fingerprint};
pub use models::{
    DiscoveredModelCache, builtin_antigravity_models, merged_antigravity_models,
    overlay_catalog_models,
};
pub use oauth::{
    AntigravityAuthorization, AntigravityTokenExchange, authorize_antigravity,
    exchange_antigravity, parse_oauth_callback_input,
};
pub use oauth_server::{
    LoopbackOAuthListener, OAuthCallbackCapture, escape_html, is_remote_environment,
};
pub use project::{ProjectContext, ensure_project_context};
pub use quota::{
    AccountQuotaFetch, DiscoveredModelInfo, fetch_account_quota, populate_hub_quota_groups,
};
pub use rotation::{
    HealthScoreTracker, RateLimitReason, TokenBucketTracker, calculate_backoff_ms,
    extract_retry_info_from_body, parse_duration_ms, parse_rate_limit_reason,
};
pub use schema::{clean_json_schema_for_antigravity, to_gemini_schema};
pub use search::{GoogleSearchOutput, GoogleSearchTool, SearchCitation, execute_google_search};
pub use service::{
    AccountStatusSummary, ActiveCredentials, AntigravityService, OAuthAttemptStatusView,
    OAuthAttemptTime, OAuthAttemptView, QuotaRefreshReport,
};
pub use signature_cache::{CachedSignatureEntry, SignatureCache, hash_thinking_text};
pub use storage::{
    AccountMetadataV4, AccountStorageV4, CURRENT_STORAGE_VERSION, CachedAccountQuota,
    QuotaGroupSnapshot, account_token_hash, default_accounts_path, load_accounts, save_accounts,
};
pub use stream::{CloudCodeSseDecoder, StreamDecodeFailure, is_thinking_order_error};
pub use token::{RefreshedAccessToken, TokenRefreshCoordinator};
pub use transform::{
    ResolvedTargetModel, ThinkingMode, TransformedRequest, build_cloud_code_request,
    resolve_antigravity_model,
};
pub use version::VersionTracker;

pub mod stream;
