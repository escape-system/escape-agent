//! Model catalog and provider streaming for opencode-rust.
//!
//! The catalog is the models.dev snapshot bundled at build time plus an optional
//! merged `providers` configuration object. The client speaks three wire
//! protocols — OpenAI-compatible chat, Anthropic Messages, and Gemini
//! `streamGenerateContent` — and reports a provider-neutral [`LlmEvent`] stream.
//!
//! No constructor panics on malformed catalog or provider input; failures are
//! [`LlmError`] or [`LlmEvent::ProviderError`].

pub mod catalog;
pub mod client;
pub mod error;
pub mod event;
pub mod hook;
pub mod key_pool;
pub mod model_health;
mod protocols;
pub mod quota;
pub mod rate_limit;
pub mod retry;
pub mod sse;
pub mod token;
pub mod vertex;

pub use catalog::{
    Activation, Capabilities, Catalog, CatalogCell, Compatibility, Cost, CostCache, CostTier,
    CostTierKind, Limit, ModelInfo, ModelRef, ModelStatus, ModelTime, Ordered, PriceOverride,
    PriceOverrideMap, Protocol, ProviderInfo, ResolvedModel, Variant, calculate_cost,
    parse_reference, parse_release_date,
};
pub use client::{LlmClient, SharedLlmClient};
pub use error::{
    FailureKind, GoogleErrorDetails, LlmError, classify_provider_message, parse_google_duration_ms,
    parse_google_error_details,
};
pub use event::{
    ContentPart, LlmEvent, LlmMessage, LlmOptions, LlmRequest, LlmStream, Role, Timeouts, ToolDef,
    Usage, model_supports_request_modalities,
};
pub use hook::{HookStreamFuture, LlmTransportHook};
pub use key_pool::{AuthDetection, ENV_KEY_ID, KeyHealthEntry, KeyPool, PoolKey, detect_auth};
pub use model_health::{ModelHealth, Unavailable};
pub use quota::{
    ProviderApiSpend, ProviderBalanceSummary, ProviderQuotaProbeResult, ProviderQuotaTarget,
    ProviderRateLimit, QuotaCapability, QuotaWindow, extract_rate_limit, fetch_provider_quota,
    fetch_provider_quotas, parse_reset_timestamp, provider_quota_capability,
};
pub use rate_limit::{ProviderRateLimiter, RateLimitDecision, RateLimitPermit, RateLimitPolicy};
pub use retry::{RetryPolicy, RetryState, parse_retry_after};
pub use sse::{SseEvent, SseParser};
pub use token::{
    CHARS_PER_TOKEN, IMAGE_TOKENS, PDF_TOKENS, estimate_message_tokens, estimate_messages_tokens,
    estimate_part_tokens, estimate_prompt_tokens, estimate_request_tokens, model_cost,
};
pub use vertex::{GoogleAdcClient, is_vertex_target, resolve_vertex_base_url};
