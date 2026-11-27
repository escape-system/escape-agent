# 26 — Antigravity OAuth & Cloud Code Assist Gateway (Legacy / Disabled by Default)

> **ToS Safety Lock**: Consumer Antigravity OAuth attachment is **disabled by default**
> in `opencode serve`, `opencode run`, `opencode chat`, and `opencode mcp` per
> [docs/30-tos-safe-harness.md](30-tos-safe-harness.md). Background maintenance timers
> default to off (`quota_refresh_interval_minutes = 0`, `proactive_token_refresh = false`,
> `auto_update = false`), and enabling legacy OAuth attachment requires an explicit
> opt-in (`OC_ALLOW_ANTIGRAVITY_OAUTH=1` or `antigravity.allow_legacy_consumer_oauth = true`).
> For autonomous harness usage, use Paid Gemini Developer API (`google`), Vertex AI ADC
> (`google-vertex`), OpenRouter, Anthropic, or inverted human-driven `opencode mcp`.

Architecture and interface contract for `oc-antigravity`, the native Rust port of
`opencode-antigravity-auth` for `opencode-rust`.

## Goals

- Authenticate Google accounts via Antigravity OAuth 2.0 + PKCE (local loopback
  callback listener on `127.0.0.1:51121/oauth-callback` or manual code/URL
  completion) and resolve Cloud Code Assist managed project context
  (`loadCodeAssist` / `onboardUser`).
- Maintain both:
  - the SQLite `credential` table in `oc-store` (`integrationID = "google"`,
    `type = "oauth"`, `methodID = "antigravity"`, packed
    `refreshToken|projectId|managedProjectId`), and
  - the multi-account pool file `antigravity-accounts.json` (v4 format with
    v1–v3 migration, SHA-256 token hashes, per-family active cursors, quota
    cache, verification flags, and device fingerprints).
- Route `google/antigravity-*` models (and `google/*` models backed by an active
  Antigravity OAuth credential) through the Cloud Code Assist Unified Gateway
  (`/v1internal:streamGenerateContent?alt=sse`) using `oc-llm::LlmTransportHook`
  on `SharedLlmClient`, preserving standard API-key Gemini behavior when an API
  key (`GEMINI_API_KEY` or `key` credential) is used on non-Antigravity models.
- Expose Antigravity model definitions (Gemini 2.5 / 3 / 3.8 / 4 Argon &
  Hydrogen + Claude Sonnet/Opus 4.5–5.5 + GPT-OSS 120B defaults, grouped
  thinking tiers `low/medium/high/xhigh`, live
  `/v1internal:fetchAvailableModels` catalog discovery auto-refreshed on OAuth
  login and startup, and dynamic runtime catalog resolution via
  `SharedLlmClient::resolve_model`) and the `google_search` grounding tool.

## Crate Placement & Boundaries

- `crates/oc-antigravity` depends only on `oc-types`, `oc-store`, `oc-config`,
  `oc-llm`, `oc-tools`, and workspace HTTP/async/crypto utilities. It **must
  not** depend on `oc-runtime` or `oc-server`.
- `oc-llm` exposes:
  - `LlmTransportHook` trait (`handles`, `stream`, `resolve_model`,
    `dynamic_models`) and hook attachment on `SharedLlmClient` / `LlmClient`
    (`with_hook`, `set_hook`, `resolve_model`, `dynamic_models`).
  - `Catalog::upsert_provider_models` to overlay `antigravity-*` and discovered
    models onto `"google"`.
- `oc-server` exposes `AppState::antigravity()` and wires:
  - `POST/GET/DELETE /api/integration/google/connect/oauth*` (real PKCE OAuth
    attempts when `methodID = "antigravity"`, returning 501 only for engines not
    yet attached such as remote MCP OAuth).
  - `/api/antigravity/accounts*`, `/api/antigravity/quotas`, and
    `/api/antigravity/models/refresh`.
  - `AntigravityToolProvider` (`oc_runtime::ToolProvider`) providing
    `google_search`.
- `oc-cli` provides `opencode auth <login|list|status|quota|use|enable|disable|remove|models>`
  and `/antigravity` (`/ag`) in interactive chat, and wires `AntigravityService`
  during `bootstrap`.

## Multi-Account Rotation & Failover

- **Strategies**: `sticky`, `round-robin`, and `hybrid` (default: deterministic
  score combining `HealthScoreTracker`, `TokenBucketTracker`, LRU recency, and
  optional PID offset).
- **Per-family & Per-quota Rate Limits**: Tracks reset timestamps separately
  for `claude`, `gemini-antigravity[:model]`, and `gemini-cli[:model]`.
- **Dual Quota Pool (Gemini)**: Respects explicit `-antigravity` / `-cli`
  routing or `cli_first`, and fails over between Antigravity Hub
  (`daily-cloudcode-pa.googleapis.com`) and Gemini CLI
  (`cloudcode-pa.googleapis.com`) before blocking.
- **Retry & Recovery**:
  - `MODEL_CAPACITY_EXHAUSTED` / `SERVER_ERROR`: exponential backoff (1 s → 8 s,
    up to 3 attempts per endpoint) plus fingerprint regeneration.
  - `RATE_LIMIT_EXCEEDED` / `QUOTA_EXHAUSTED`: respects `Retry-After` /
    `google.rpc.RetryInfo`, honors `cache_first` waits up to
    `max_cache_first_wait_seconds`, otherwise switches accounts or falls back to
    alternate quota pool.
  - `403 validation_required`: extracts verification URL, flags the account as
    `verificationRequired` and disables it, then switches to the next healthy
    account.
  - Empty SSE streams / `thinking_block_order` 400 errors: retries or applies
    `close_tool_loop_for_thinking` recovery when enabled.

### In-band quota frames (shipped)

Cloud Code can report exhaustion *inside* an HTTP 200 SSE stream as a
`{"error":{"message":…}}` frame, so the message decides the class, not the HTTP
status. The decoder returns `StreamDecodeFailure::{QuotaExhausted, RateLimited}`
and `in_band_or` maps them to `SingleAttempt::RateLimited { status: 200 }`, which
makes the pool mark the serving account and rotate to the next one instead of
failing the turn with raw provider text
(`crates/oc-antigravity/src/gateway/stream.rs:122-146`). A frame without account
attribution stays fatal with its verbatim text. Once a stream has emitted its
first event, rotation is impossible and the mid-stream message names the
account-level cause instead (`gateway/stream.rs:154-171`).

`parse_rate_limit_reason` recognises `RESOURCE_EXHAUSTED`, `RESOURCE EXHAUSTED`,
`RESOURCE HAS BEEN EXHAUSTED`, `QUOTA_EXHAUSTED`, `INSUFFICIENT_QUOTA`,
`MODEL_CAPACITY_EXHAUSTED`, `DAILY LIMIT` and `CHECK QUOTA` (case-insensitive) as
`QuotaExhausted`, ahead of the generic 429 arm — a depleted quota window needs
the longer cooldown, not a burst ceiling (`crates/oc-antigravity/src/rotation.rs:37-73`,
asserted by `resource_exhausted_is_quota`, `rotation.rs:513`). Consequently
`classify_in_band_error` maps `QuotaExhausted` **and** `ModelCapacityExhausted`
onto `StreamDecodeFailure::QuotaExhausted`, and only `RateLimitExceeded` onto
`StreamDecodeFailure::RateLimited` (`crates/oc-antigravity/src/stream.rs:253-272`).

Cooldown keys are per account, per quota bucket, and **variant-insensitive**:
`quota_key(family, style, model)` strips the `#variant` suffix and lowercases the
model, so `model#high` and `model#xhigh` share one bucket
(`crates/oc-antigravity/src/accounts.rs:51-68`). A `QuotaExhausted` result on a
model writes the model key *and* the family base key (no model), because the
quota stop is account-wide; other reasons write only the model key
(`accounts.rs:568-590`).

When no account can serve the request the gateway returns a typed error rather
than provider prose (`crates/oc-antigravity/src/gateway/pool.rs:126-176`), e.g.
`all Antigravity accounts are cooling down until <untilMs> (every eligible
account already failed this request)` and `… (next reset in Ns, exceeds the
per-turn wait budget)`, plus
`no usable Google Antigravity accounts configured; run "opencode auth login"`.
Asserted by `gateway_rotates_on_in_band_quota_error`
(`crates/oc-antigravity/src/gateway/mod.rs:369`) and
`all_accounts_cooling_down_is_reported` (`gateway/mod.rs:470`).

Background maintenance runs in the served binary: `bootstrap` calls
`AntigravityService::spawn_background_maintenance()`
(`crates/oc-cli/src/bootstrap.rs:238`), which spawns the tick loop for proactive
token refresh and periodic quota/model refresh unless both
`proactive_token_refresh` is off and `quota_refresh_interval_minutes == 0`
(`crates/oc-antigravity/src/service/mod.rs:268-297`; the disabled path is
asserted by `background_maintenance_is_a_noop_when_disabled`,
`crates/oc-antigravity/src/service/tests.rs:182`).

## Request & Stream Transformation Pipeline

1. **Model resolution**: Maps `antigravity-*` aliases, tier suffixes
   (`-low`/`-medium`/`-high`/`-minimal`), variants, and dynamic advertised IDs.
2. **Schema cleaning**: 4-phase JSON Schema sanitizer (`$ref` hint, `const` →
   `enum`, constraint hints, `allOf` merge, `anyOf`/`oneOf` flattening,
   type-array flattening, unsupported keyword stripping, empty-object
   `_placeholder`), plus `to_gemini_schema` uppercase types for Gemini models.
3. **Claude transforms**: `toolConfig.functionCallingConfig.mode = "VALIDATED"`,
   snake_case `thinking_config`, `maxOutputTokens >= 64000`, tool name
   sanitization, optional `claude_tool_hardening` parameter hints + system
   instruction, `functionCall` ID assignment & FIFO `functionResponse` pairing,
   cross-model signature sanitization, and thinking signature cache /
   `skip_thought_signature_validator` sentinel injection.
4. **Envelope & SSE Decoding**: Wraps into `{ project, model, request, userAgent,
   requestType, requestId }`, sends `Accept-Encoding: identity` with 30 s connect
   and 300 s read timeouts plus HTTP/2 and TCP keep-alives, decodes
   `{ response: { candidates, usageMetadata } }` SSE frames (including mixed
   `\r\n`/`\n` frame boundaries, UTF-8 multi-byte chunk boundaries,
   `promptFeedback.blockReason`, and nested `response.error` payloads) into
   `oc_llm::LlmEvent` (`ToolInputStart`/`Delta`/`End`), deduplicates cumulative
   thinking deltas, and caches returned `thoughtSignature` values (including on
   `functionCall` parts). Live `/v1internal:fetchAvailableModels` discovery
   updates `limit.context` and `limit.output` on built-in specs as well as adding
   newly discovered models, and cold-start cached catalog loads re-apply the
   Antigravity overlay.

## Credential Key Compatibility

- OAuth credentials in `oc-store` follow OpenAPI's `Credential.OAuth` shape
  (`{ type: "oauth", methodID, refresh, access, expires, metadata? }`).
- Runtime, observer, and scribe `credential_key` resolution check `access`
  before legacy `access_token`, and session export sanitization strips `access`
  and `refresh` alongside `access_token` and `refresh_token`.
