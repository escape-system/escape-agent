# 30 — ToS-Safe Harness Hardening, Official Google Cloud / Gemini Routing, Security & MCP Server Mode

This specification locks down the architectural changes ensuring `opencode-rust` operates completely free of consumer Antigravity OAuth ToS risks while aligning the Rust harness with production reliability, financial safety, OS sandboxing, observability, and inverted-control MCP usage.

## 1. Zero Consumer Antigravity OAuth Risk by Default

- **Autonomous harness lock**: `opencode serve`, `opencode run`, and `opencode chat` must **never** open `oc-antigravity`, load `~/.config/opencode/antigravity-accounts.json`, spawn background version checks or quota/token refresh loops, attach `AntigravityTransportHook`, or expose `AntigravityToolProvider` by default.
  - Only an explicit legacy opt-in (`OC_ALLOW_ANTIGRAVITY_OAUTH=1` or `antigravity.allow_legacy_consumer_oauth: true` in config) re-enables attachment, logging a clear warning.
  - `AntigravityConfig::default()` sets `quota_refresh_interval_minutes = 0`, `proactive_token_refresh = false`, and `auto_update = false` so unit/integration usage never launches unattended daemon loops unless explicitly configured.
- **Credential separation (`google`)**: When no Antigravity transport hook handles a request, `Runtime::credential_pool_for("google", ...)` ignores stored credentials of `type == "oauth"` so an Antigravity OAuth blob is never sent as `x-goog-api-key` to `generativelanguage.googleapis.com`.
- **Auth CLI safety (`opencode auth login`)**: `login --provider google` stores Paid Tier API keys (`--api-key` / `GEMINI_API_KEY`). Consumer Antigravity OAuth flows require explicit `--allow-consumer-oauth` opt-in (or `OC_ALLOW_ANTIGRAVITY_OAUTH=1`) and surface a clear ToS warning directing autonomous workloads to Gemini API (`google`), Vertex AI (`google-vertex`), OpenRouter (`openrouter`), Anthropic (`anthropic`), or `opencode mcp`.

## 2. Official Google Routes & Frontier Catalog (`oc-llm`)

### Paid Gemini Developer API (`google`) & Interactions API (`Antigravity Managed`)
- `apply_builtin_providers` ensures frontier Google models exist offline alongside snapshot entries with Dec 1 pricing/routing metadata:
  - `google/gemini-4-neon` (`$3/$15`, `$0.15` cache read, 1M context, `thinkingConfig.thinkingLevel` `low|medium|high|xhigh`, multimodal text/image/audio/video/pdf)
  - `google/gemini-4-carbon` (`$4/$20`, `$0.20` cache read)
  - `google/gemini-4-hydrogen` (`$1.50/$7.50`, `$0.075` cache read, high-speed coding executor)
  - `google/antigravity-preview-09-2026` (`Protocol::GeminiInteractions`, routing through official Paid API `POST /interactions` with default `store: false`)
- Privacy `store: false`: supported cleanly on `LlmOptions::store` (plus provider settings) for Gemini Interactions API and privacy-sensitive API requests.

### Google Cloud Vertex AI (`google-vertex`) — ADC & Dual Gemini 4 + Claude 5.5
- Placeholder expansion for `google-vertex` `baseURL`: resolves `${GOOGLE_VERTEX_ENDPOINT}` (defaults to `aiplatform.googleapis.com` when location is `global`, else `{location}-aiplatform.googleapis.com`), `${GOOGLE_VERTEX_PROJECT}` (`GOOGLE_VERTEX_PROJECT` -> `GOOGLE_CLOUD_PROJECT` -> `GCLOUD_PROJECT` -> ADC JSON `project_id`), `${GOOGLE_VERTEX_LOCATION}` (defaults to `global`), and `${GOOGLE_VERTEX_CLAUDE_ENDPOINT}` (`claude.googleapis.com` opt-in).
- Pure-Rust safe (`unsafe_code = "forbid"`) GCP Application Default Credentials (`google_adc`):
  - Supports explicit `GOOGLE_VERTEX_ACCESS_TOKEN`, Service Account JSON (`GOOGLE_APPLICATION_CREDENTIALS` -> RS256 JWT assertion signed with `ring::signature::RsaKeyPair::from_pkcs8` exchanged at `token_uri`), and `authorized_user` JSON (`refresh_token` exchange at `oauth2.googleapis.com/token`).
  - Cached access tokens are held in `zeroize::Zeroizing<String>` with refresh buffer before expiry and custom `Debug` redaction.
  - Vertex requests authenticate with `Authorization: Bearer <token>`; Vertex Anthropic routes inject `"anthropic_version": "vertex-2023-10-16"` cleanly for `publishers/anthropic/models/*:streamRawPredict` paths.

## 3. Resilience, Gemini Error Intelligence, Proactive Limits & Timeouts

- **Single-Owner Retry Rule**: `Runtime` configures its `LlmClient` with `RetryPolicy::single_attempt()` (`max_attempts: 1`, `max_timeout_retries: 0`) so `LlmClient` never multiplies retries underneath `Runtime::stream_model_attempts`. Standalone `LlmClient::new()` retains its existing default `RetryPolicy`.
- **Gemini `error.details` (`RetryInfo` & `QuotaFailure`)**:
  - `LlmClient::attempt` parses JSON `error.details[]` on HTTP errors:
    - `google.rpc.RetryInfo.retryDelay` populates `retry_after_ms` cleanly (combined with positive jitter as a sleep floor).
    - `google.rpc.QuotaFailure` with permanent indicators (`limit: 0`, `quotaValue == "0"`, `enforced_spend_limit_reached`, billing depletion) marks errors terminal (`is_retryable() == false`) so retry loops abort immediately.
- **Policy Block Classification (`FailureKind::PolicyBlocked`)**:
  - `oc-llm/src/protocols/gemini.rs` inspects `promptFeedback.blockReason` and safety `finishReason` values (`SAFETY`, `RECITATION`, `BLOCKLIST`, `PROHIBITED_CONTENT`, `SPII`).
  - Emits typed non-retryable `FailureKind::PolicyBlocked` (`policy_blocked`) instead of returning empty text or retrying.
- **Proactive GCRA Rate Limiting (`governor`) & Tier 1 Cost Velocity Guard**:
  - `oc-llm::rate_limit::ProviderRateLimiter` uses `governor::RateLimiter` (GCRA) for configurable RPM/TPM throttling plus a 10-minute sliding spend velocity tracker (`tier1_spend_cap_usd`), automatically dropping concurrency guards before sleeping on 429 waits.
- **Tiered Dynamic Timeouts & Modality Guard**:
  - High/max reasoning variants (`xhigh`, `max`) automatically raise `Timeouts.header` to `600s` (covering ~338s Opus `#max` TTFT) and `high` to `420s`, while `low`/`medium` stay responsive (`120s`).
  - Modality compatibility check (`model_supports_parts`) prevents falling back a request carrying audio/video parts onto text+image-only models.

## 4. Secret Hygiene, Affine `CostBudget`, Thinking Cap, SHA-256 Loop Guard & Telemetry

- **Memory hygiene (`zeroize`)**: Credential secrets (`PoolKey.key`, ADC tokens, private keys) wrap `zeroize::Zeroizing<String>`. `detect_auth()` reports configured credential availability cleanly without reading or printing secret material.
- **Affine `CostBudget` (`oc-subagents`)**: Non-`Clone`, non-`Copy` ownership-tracked budget in micro-cents (`1 uc = $0.00001`): `reserve(estimated_uc)` (`checked_sub`), `settle(reservation, actual_uc)`, `split(amount)` (move to subagent), and `reclaim(child)` guarantees zero concurrent double-spend across parallel subagent trees at compile time.
- **Thinking Level Cap**: `models.thinking_cap` / agent overrides clamp excessive `#xhigh`/`#max` tiers down to policy limits deterministically.
- **SHA-256 Loop Guard (`stall.rs`)**: Deterministic SHA-256 canonical digest over `(tool_name, sorted_canonical_json_args)` plus consecutive failing output digest tracking stops unproductive identical loops cleanly.
- **Prefix-Stable Microcompaction**: Deterministically summarizes/compacts oversized stale `ToolResult` entries prior to full LLM compaction without mutating live KV-cache prefixes mid-turn.
- **OTel `gen_ai.*` Observability**: Structured `tracing` spans emit `gen_ai.system`, `gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.usage.*`, `gen_ai.response.finish_reasons`, and latency/cost metrics without logging raw prompts or secrets.

## 5. OS Kernel Sandboxing, Deterministic Policy Hooks & MCP Server Mode (Road C)

- **Shell Sandbox (`oc-tools/src/sandbox.rs`)**: Configurable `off | bwrap | auto` Linux Bubblewrap (`bwrap`) confinement binding root read-only (`--ro-bind / /`), workdir + `/tmp` read-write, `--die-with-parent`, `--new-session`, optional `--unshare-net`, and automatic secret-env scrubbing (`GEMINI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_VERTEX_ACCESS_TOKEN`, etc.).
- **Deterministic Tool Security Hooks (`oc-runtime`)**: `pre_tool_use` denies file/shell attempts targeting known secret stores (`antigravity-accounts.json`, `escape.db`, credentials JSON), and `post_tool_use` redacts detected secret token shapes before returning tool text to the LLM.
- **MCP Server Mode (`opencode mcp`)**: Built on `rmcp 3.12.0` (`server` + `transport-io`, zero Streamable HTTP surface exposed) exposing deterministic workspace tools (`read`, `glob`, `grep`, `code_map`, `symbol_edit`, `edit`, `patch`, `write`, `shell` — with `--read-only` flag) over `stdio` so human-driven official Antigravity App / IDE sessions can leverage Rust tools safely without OAuth bridges or external task-loop orchestration.
