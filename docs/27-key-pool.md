# 27 — Provider key pools and key rotation

Every provider may hold several credentials ("backup keys"). When one key is
exhausted the runtime rotates to the next key **on the same model**; the model
fallback chain ([05](05-subagents.md)) is untouched. This document is the
rotation contract; the config surface is [14](14-config-permissions.md), the
routes [02](02-protocol.md).

## Credential rows

A credential is a row in the SQLite `credential` table, owned by an integration:

```sql
credential(id TEXT PRIMARY KEY, integration_id TEXT, label TEXT,
           value TEXT, active INTEGER);
```

- No schema change is needed for backup keys: `integration_id` is not unique
  (`crates/oc-store/src/schema.rs:192-198`), so N rows per provider are legal
  and one `cred_…` id identifies one key.
- **At most one active row per integration** is a code invariant, not a schema
  constraint. `create_credential`, `update_credential`, `activate_credential`
  and `set_credential_primary` clear the sibling flags inside the same
  transaction (`crates/oc-store/src/credentials.rs:115-120`, `:373-391`).
- `value` stays opaque tagged JSON (`oauth` | `key` | `external`); the key
  material never leaves the store except as an `Authorization` / `x-api-key` /
  `x-goog-api-key` header (`crates/oc-llm/src/client.rs:174-198`).

## Ordering

`Store::credential_pool(integration_id)` is the single source of truth for
rotation order (`crates/oc-store/src/credentials.rs:303-331`):

```sql
SELECT … FROM credential WHERE integration_id = ?1 ORDER BY active DESC, id ASC
```

The active row comes first, then the remaining ids ascending. Ids encode
creation time, so the order is stable across restarts with no extra column.
`Store::set_credential_primary(integration_id, credential_id)` moves the active
flag via the same transaction body as `activate_credential`; an unknown or
foreign id returns `false` and writes nothing
(`crates/oc-store/src/credentials.rs:333-366`). Both properties are covered by
`credential_pool_returns_active_first_then_id_ascending` and
`set_credential_primary_clears_siblings_and_survives_reopen`
(`crates/oc-store/tests/store_ext/credentials.rs:195-310`).

## Cooldowns

`oc_llm::KeyPool` owns the process-local rotation table
(`crates/oc-llm/src/key_pool.rs:88-194`), exported as
`oc_llm::{KeyPool, PoolKey, KeyHealthEntry, ENV_KEY_ID}`
(`crates/oc-llm/src/lib.rs:37`). It is keyed `provider -> credential id`, where
a key that came from the environment is tracked under `ENV_KEY_ID = "__env__"`
(`crates/oc-llm/src/key_pool.rs:37`).

| Failure class | Cooldown |
|---|---|
| `quota` | 5 min |
| `rate_limit` | 60 s |
| `auth` | 30 min |
| `transport` | 30 s |
| `model_unavailable`, `invalid_request`, `empty_response`, `unknown` | not remembered |

(`crates/oc-llm/src/key_pool.rs:200-211`.)

Semantics:

- `select(provider, keys, now_ms)` keeps the caller's order and drops keys whose
  cooldown is still in the future; an entry is usable again at exactly
  `until_ms`. Expired rows for that provider are pruned on the way out
  (`:100-125`).
- `mark` never shortens an existing longer cooldown, always increments the
  per-key failure counter, and is a no-op for the model/request-scoped classes
  above (`:127-163`).
- `entries(now_ms)` returns the unexpired snapshot pruned and sorted by
  provider then credential id; `clear()` forgets everything (config reloads,
  explicit repair) (`:165-193`).
- Provider ids compare case-insensitively (`:229-231`), so `Google` and
  `google` are one pool.

## Rotation seam

Rotation belongs where the request is rebuilt per attempt, not inside the HTTP
client. `Http { status: 429 }` retry/backoff in `oc-llm::client` stays as it is:
the pool sits strictly above it.

`Runtime::stream_step` runs the model loop and calls
`Runtime::stream_model_attempts` once per model candidate
(`crates/oc-runtime/src/turn/stream.rs:150-235`, `:237-355`). Inside one
candidate the order is:

1. Build the ordered candidate keys for the provider — the active credential
   first (`Store::credential_pool`), then the remaining ids, plus the provider's
   environment key as its own slot when it adds material no stored row provides
   (`Runtime::credential_pool`, `crates/oc-runtime/src/turn/session.rs:39-80`).
2. Filter through `KeyPool::select` and try the first usable key.
3. On `Auth` / `Quota` / `RateLimit` (an `Http` 401/402/403 maps to `Auth`, 429
   to `RateLimit`), `mark` that key and retry the **same** model reference with
   the next usable key, reusing the same assistant row when nothing streamed.
   Every other class returns immediately: a model- or request-scoped failure is
   not a key problem (`key_failure_kind`, `:61-75`).
4. Only when **every** key for that model is exhausted does the last failure
   return to `stream_step`, which runs the existing model chain advance exactly
   once.

`MAX_KEY_ATTEMPTS = 8` bounds the work per candidate even if a pool reports more
keys (`crates/oc-runtime/src/turn/stream.rs:51`, `:300`). An empty pool is not a
key problem: the request is attempted exactly once with `api_key: None` and the
client resolves the provider environment itself (`:293-301`).

Hard rules:

- A key rotation MUST NOT publish `session.model.fallback`
  (`crates/oc-types/src/payload/meta.rs:55`). That event means the model
  changed; the runtime publishes it only inside the chain-advance arm
  (`crates/oc-runtime/src/turn/stream.rs:209-227`).
- Rotation is per key, not per model: an exhausted key says nothing about
  whether the next key can serve the same model, and a revoked key must rotate
  away from that key only (`crates/oc-llm/src/key_pool.rs:3-10`).
- When the marked key was primary, the durable pointer moves to the next
  healthy key (`Store::set_credential_primary`) so a restart does not pick the
  dead key first. `mark_key_failure` only moves it when the failing id is
  `rows[0]` and leaves the store untouched otherwise
  (`crates/oc-runtime/src/turn/stream.rs:363-405`). Cooldowns themselves are
  **not** durable.
- A key that streamed output stays durable as a failed attempt row: the next
  key starts a new assistant row rather than replaying partial output
  (`:326-346`). When nothing streamed the same row is reused, so a rotation
  never duplicates a `session_message` row.
- Compaction does not rotate: it reuses one shortened request, so it tries the
  first usable key once and only records a cooldown
  (`mark_compaction_key`, `crates/oc-runtime/src/turn/compaction.rs:190-220`,
  `:435-470`). The next turn then picks a different key.

## Antigravity accounts

An Antigravity account is one credential row plus one entry in the account pool
([26](26-antigravity-auth.md)). Selection excludes accounts with an active
`rate_limit_reset_times` entry for the model's quota key **or** its family base
key (`crates/oc-antigravity/src/accounts.rs:326-339`), and `mark_rate_limited`
writes both entries for `QuotaExhausted`
(`crates/oc-antigravity/src/accounts.rs:559-585`).

The in-band rule (shipped): Cloud Code can report quota exhaustion **inside a
200 SSE stream** as `{"error":{"message":"Resource has been exhausted (e.g.
check quota)."}}`. The status code is therefore not the classifier — the message
is. `handle_json_frame` now classifies such a frame with the shared reason
parser instead of failing:

- `StreamDecodeFailure` gained `QuotaExhausted(String)` and
  `RateLimited(String)` beside `EmptyStream` / `ThinkingBlockOrder` / `Fatal`
  (`crates/oc-antigravity/src/stream.rs:248-256`).
- `classify_in_band_error` runs `parse_rate_limit_reason(200, message)`:
  `QuotaExhausted` and `ModelCapacityExhausted` become
  `StreamDecodeFailure::QuotaExhausted`; `RateLimitExceeded` becomes
  `RateLimited`; `ServerError`/`Unknown` stay `Fatal`, because they carry no
  useful account attribution (`crates/oc-antigravity/src/stream.rs:263-275`).
- `parse_rate_limit_reason` matches `RESOURCE_EXHAUSTED`, `RESOURCE EXHAUSTED`,
  `RESOURCE HAS BEEN EXHAUSTED`, `QUOTA_EXHAUSTED`, `INSUFFICIENT_QUOTA`,
  `MODEL_CAPACITY_EXHAUSTED`, `DAILY LIMIT` and `CHECK QUOTA`
  case-insensitively as `QuotaExhausted`, and it does so for **any** status
  including 200 (`crates/oc-antigravity/src/rotation.rs:39-71`; asserted by
  `resource_exhausted_is_quota`, `crates/oc-antigravity/src/rotation.rs:512`).
- `in_band_or` maps those two variants to `SingleAttempt::RateLimited { status:
  200 }`, and `SingleAttempt::RateLimited` calls `mark_rate_limited`, excludes
  the serving account and rotates to the next one
  (`crates/oc-antigravity/src/gateway/stream.rs:125-146`,
  `crates/oc-antigravity/src/gateway/single.rs:74-107`).

Cooldown key scheme: one key is `<family base>[:<model>]`, e.g. `gemini-antigravity`,
`gemini-antigravity:gemini-3.1-pro`, `claude`, or `gemini-cli:…`
(`quota_key`, `crates/oc-antigravity/src/accounts.rs:53-70`). It is
variant-insensitive: the model suffix drops everything from `#` (`model#high`
and `model#xhigh` share one upstream bucket, lowercased). `QuotaExhausted` on a
model therefore writes the model key **and** the family base key, so a depleted
quota window also pauses the other models of that family; a burst
`RateLimitExceeded` writes only the model key (`:573-587`).

All-accounts-cooling is a typed error, never raw provider text
(`crates/oc-antigravity/src/gateway/pool.rs:126-176`):

- `no usable Google Antigravity accounts configured; run "opencode auth login"`
  (`:139`)
- `all Antigravity accounts are cooling down until <untilMs> (every eligible
  account already failed this request)` (`:154-157`)
- `all Antigravity accounts are cooling down until <untilMs> (next reset in Ns,
  exceeds the per-turn wait budget)` (`:167-172`)

Background maintenance (proactive token refresh + periodic quota/model refresh)
is actually started: `bootstrap` calls `AntigravityService::spawn_background_maintenance()`
(`crates/oc-cli/src/bootstrap.rs:238`), which spawns the tick loop unless both
`proactive_token_refresh` is off and `quota_refresh_interval_minutes == 0`
(`crates/oc-antigravity/src/service/mod.rs:268-297`; the disabled path is
asserted by `background_maintenance_is_a_noop_when_disabled`,
`crates/oc-antigravity/src/service/tests.rs:182`).

## Error surface

When every key of a provider is cooling down and no further candidate exists,
the turn fails with the last provider error; the rotation layer itself invents
no success. The all-cooling-down message names the next reset time rather than
repeating the upstream sentence.

## Durability

- `credential` rows and the `active` flag are durable: kill -9 then restart
  yields the same pool order (`crates/oc-store/tests/store_ext/credentials.rs:235-310`).
- Cooldowns are process-local and reset on restart by design: after a restart
  the pool falls back to priority order, exactly like `ModelHealth::clear`
  (`crates/oc-llm/src/key_pool.rs:25-28`).
- Key material is never formatted and never persisted outside the credential
  row: `PoolKey`'s `Debug` prints `<redacted>`
  (`crates/oc-llm/src/key_pool.rs:53-60`).

## Test matrix

| Layer | Test | Asserts |
|---|---|---|
| `oc-store` | `credential_pool_returns_active_first_then_id_ascending` | active first, then id ascending, exactly one active |
| `oc-store` | `set_credential_primary_clears_siblings_and_survives_reopen` | siblings cleared in one transaction; unknown/foreign id refused; order survives reopen |
| `oc-store` | `deleting_the_active_credential_leaves_the_rest_of_the_pool` | the backup key remains usable |
| `oc-llm` | `select_drops_cooled_keys_and_keeps_caller_order` | caller order preserved; usable again at `until_ms` |
| `oc-llm` | `each_class_uses_its_own_cooldown` | 300 s / 60 s / 1800 s / 30 s |
| `oc-llm` | `model_and_request_scoped_kinds_are_never_marked` | model-level failures leave the key usable |
| `oc-llm` | `shorter_marks_never_shorten_a_longer_cooldown` | monotone cooldown, failure counter |
| `oc-llm` | `entries_prune_expired_and_sort`, `clear_forgets_everything` | snapshot pruning / reset |
| `oc-llm` | `env_keys_are_tracked_separately_from_stored_keys` | `__env__` is its own slot |
| `oc-llm` | `provider_ids_compare_case_insensitively`, `debug_redacts_key_material` | normalization, redaction |
| `oc-runtime` | `quota_on_the_active_key_rotates_to_the_backup_key` (`crates/oc-runtime/tests/runtime/key_pool.rs:51`) | key A exhausted -> key B serves the same model, 2 provider calls, 0 `session.model.fallback` |
| `oc-runtime` | `all_keys_exhausted_advances_the_chain_once` (`key_pool.rs:112`) | the chain advances exactly once, and only after the pool is spent |
| `oc-runtime` | `auth_failure_rotates_without_chain_advance` (`key_pool.rs:165`), `auth_failure_on_two_keys_does_not_advance_the_chain` (`crates/oc-runtime/tests/runtime/models_policy.rs:554`) | a permanent class rotates the key and never advances the chain, even across two keys |
| `oc-runtime` | `rotate_uses_the_same_model_reference` (`key_pool.rs:198`) | the retry re-resolves the *same* model reference; rotation is not a model fallback |
| `oc-antigravity` | `gateway_rotates_on_in_band_quota_error` (`crates/oc-antigravity/src/gateway/mod.rs:369`) | `RESOURCE_EXHAUSTED` frame rotates accounts instead of failing the turn |
| `oc-antigravity` | `all_accounts_cooling_down_is_reported` (`gateway/mod.rs:470`) | every account cooling -> typed error naming the next reset, not raw provider text |
| `oc-antigravity` | `resource_exhausted_is_quota` (`crates/oc-antigravity/src/rotation.rs:513`) | `RESOURCE_EXHAUSTED` / "resource has been exhausted" classify as `QuotaExhausted` |

## Shipped wire surfaces

Rotation is observable, but it owns no schema, route or config key: the pool is
derived from the credential rows. Four read-only surfaces expose its state, and
the model itself can read the credential axis through `key_health`
(`docs/17-interfaces.md`).

- `GET /api/quotas` — each provider entry carries `keys[]`, one entry per stored
  credential in rotation order, so `keys[0]` is the key a request tries first:
  `credentialID`, `label`, `active`, `limited`, `cooldownUntilMs?`, `kind?`,
  `failures?`, `usedCostUsd`, `usedRequests`. The array is omitted (never `null`)
  when the provider owns no credential row
  (`crates/oc-server/src/data/quotas/types.rs:66-71`,
  `crates/oc-server/src/data/quotas/aggregate.rs:180-212`).
- `GET /api/models/registry` — a top-level `keyHealth[]` of
  `{providerID, credentialID?, untilMs, kind, failures}`, omitted when every key
  is healthy. This is the credential axis: it never hides a model, it only takes
  one key out of rotation for one provider
  (`crates/oc-server/src/data/models_registry.rs:88-116`).
- `GET /api/antigravity/accounts` — `cooldownUntilMs` per account is the maximum
  deadline still in the future, omitted (never `null`) for a healthy account
  (`crates/oc-server/src/data/antigravity.rs:151-160`).

## Model-facing surface

Rotation is silent, so the model is told about it in two places instead of
guessing.

- **`key_health` tool** (`crates/oc-tools/src/key_health.rs`, source in
  `crates/oc-runtime/src/key_health.rs`): read-only, no required arguments, one
  optional exact `provider` filter. It returns
  `{summary, providers: [{providerID, keys: [{credentialID?, label?, active,
  limited, cooldownUntilMs?, kind?, failures?, source?}]}]}` — the same join
  `/api/quotas` performs, without the spend columns. Every key of every provider
  (or of the filtered one) is listed, including a provider whose keys are all
  healthy, so the agent can answer "2 keys for opencode-go, both healthy". The
  environment key is `source: "env"` with no `credentialID`; cooldown fields are
  omitted unless `limited` is true. Key material has no field in the result
  type, so it cannot leak through `output` or `metadata`.
- **`<key-health>` prompt line** (`crates/oc-runtime/src/key_health.rs`,
  appended at the end of the system prefix in `turn/session.rs`
  `refresh_instructions`): present **only** while at least one key is cooling
  down, absent for a healthy pool so the cached prefix does not churn on every
  rotation. It names the provider, the failure class, the soonest deadline
  (`HH:MM UTC`) and how many keys remain usable, and asks the model to say which
  key it switched to. Capped at 300 characters; a wider aggregate falls back to
  the single-line form. It never contains a credential id or key material.

Limitation, deliberate and documented in the code: `usedCostUsd` and
`usedRequests` are **always 0** (`crates/oc-server/src/data/quotas/aggregate.rs:206`).
The usage rollup keys on `model.providerID` alone, so no per-key attribution
exists; the fields stay on the wire so a client can render the column without a
second shape once attribution lands. A zero is reported rather than a guessed
split.
