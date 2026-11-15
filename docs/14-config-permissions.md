# 14 — Config and permissions

Wire-compatible with upstream v2 config loading, including V1 alias
translation. Existing user configs must keep their behavior; new keys are
additive and namespaced.

## Files and roots

- `opencode.json` and `opencode.jsonc` (JSONC), both parsed.
- Global config: `$XDG_CONFIG_HOME/opencode` (fallback `~/.config/opencode`).
- Data: `$XDG_DATA_HOME/opencode` (fallback `~/.local/share/opencode`);
  state/cache analogous.
- System/managed: `/Library/Application Support/opencode` (macOS),
  `%ProgramData%\opencode` (Windows), `/etc/opencode` (Linux); macOS MDM
  `ai.opencode.managed`.
- Project discovery walks upward from the location directory collecting
  `opencode.json(c)`, `.opencode/`, `.claude/`, `.agents/`.
- `.claude`/`.agents` feed skills/agents compatibility only.

Load order (low → high):

1. managed policy statements (reversed)
2. Console/integration documents
3. well-known/remote documents
4. global dir files + `Directory`
5. `OPENCODE_CONFIG` explicit file
6. ancestor direct files
7. project `.opencode` dirs
8. `OPENCODE_CONFIG_CONTENT`
9. managed settings/system dir + MDM

`Config.latest(entries,key)`: the last document defining a key replaces the
whole value. Map-like keys (`agents`, `providers`, `mcp.servers`, `commands`,
`policies`) are folded per document by their plugins, not deep-merged.

Text substitution before parse: `{env:VAR}` (process env, or well-known
credential) and `{file:path}` (relative to the config dir, `~/` expands,
missing ⇒ empty, `//` comment lines skipped). Substitution is textual and
escaped only when the JSON would break.

Writes: settings write only explicitly patched keys, atomically, to the
highest-precedence existing global file (`opencode.jsonc` preferred). A
user-supplied file's mode is never changed.

## V1 aliases (must keep working)

`provider→providers`, `agent`/`mode→agents`, `plugin→plugins`,
`permission`+`tools→permissions`, `command→commands`, `reference→references`,
`attachment→media`, `snapshot→snapshots`, `autoupdate→update`,
`autoshare→share`, `small_model→agents.title.model`,
`enabled_providers`/`disabled_providers→policies`,
`experimental.mcp_timeout→mcp.timeout`.

Action renames: `write|patch|apply_patch→edit`, `task→subagent`, `bash→shell`.
Dropped with a warning (not an error): top-level `logLevel`, `server`,
`subagent_depth`, `layout`; `experimental.disable_paste_summary`,
`batch_tool`, `openTelemetry`, `primary_tools`, `continue_loop_on_deny`.

## Canonical keys

| Key | Type | Default |
|---|---|---|
| `$schema` | string | — |
| `shell` | string | platform default |
| `model` | `"provider/model[#variant]"` or `{providerID,model,variant?}` | first configured |
| `models` | `{chain?,chains?,fallback?,notes?,agent_notes?,hidden?,favorites?,pricing?,quota?}` | absent: no chain, no quota, no price override |
| `default_agent` | string | `engineer` (legacy `build` is normalized to it) |
| `update` | `disable\|notify\|auto` | channel |
| `share` | `manual\|auto\|disabled` | `manual` (inert) |
| `enterprise` | `{url?}` | — |
| `username` | string | OS user |
| `timezone` | IANA | server local |
| `permissions` | `{action,resource,effect}[]` | agent defaults |
| `policies` | `{action:"provider.use"\|"tool.use"\|"integration.use",resource,effect:"allow"\|"deny"}[]` | none |
| `agents` | map of `{model?,request?,system?,description?,mode?,hidden?,color?,steps?,disabled?,permissions?}` | built-ins |
| `snapshots` | bool | true |
| `watcher` | `{ignore?:string[]}` | `[]` |
| `formatter` | bool or map `{disabled?,command?,environment?,extensions?}` | auto |
| `lsp` | bool or map `{command,extensions?,disabled?,env?,initialization?}` | built-ins |
| `media` | `{image?:{auto_resize?,max_width?,max_height?,max_base64_bytes?}}` | defaults |
| `tool_output` | `{max_lines?,max_bytes?}` | 2000 / 51200 |
| `mcp` | `{timeout?,servers?}` | — |
| `compaction` | `{auto?,keep?{tokens?},buffer?,ceiling_tokens?}` | false / 15000 / 15% |
| `skills` | string[] | dirs `skill/`, `skills/` |
| `commands` | map `{template,description?,agent?,model?,subtask?}` | markdown |
| `instructions` | string[] | AGENTS.md discovery |
| `references` | map | — |
| `websearch` | false or `{provider}` | auto |
| `plugins` | `(string\|{package,options?})[]` | none |
| `worktree` | `{directory}` | — |
| `warming` | bool or `{prompt?,interval?,duration?}` | false |
| `providers` | map `Provider` | models.dev |
| `experimental` | `{portable_shell_scanner?,todo?,background_compaction?,media_blob?,image_generate?,schedules?,subagent_depth?,location_idle_timeout?,policies?}` | mostly false; depth 1; idle 60 min |

`Provider`/`Model` fields and `Provider.Request` remain exactly as upstream
(`settings`, `headers`, `body`, `compatibility`, `capabilities`, `variants`,
`cost[]`, `limit`). Model ref syntax `provider/id[#variant]` is parsed in one
place; unknown variants are rejected with the list of available ones.

`providers` also stays upstream-shaped for credentials: backup keys are **not**
a config key. A provider's keys are `credential` rows (one row per key), and the
config only names providers and their environment variables
(`providers.<id>.env`).

#### Backup keys (several credentials per provider)

- Multiple credentials per provider are supported; they are ordinary
  `credential` rows sharing one `integration_id`. No schema change and no
  config flag: every non-active row is a backup.
- **At most one active row per integration** is a code invariant enforced
  transactionally (`create_credential`, `update_credential`,
  `activate_credential`, `set_credential_primary` clear the sibling flags inside
  one transaction — `crates/oc-store/src/credentials.rs:115-120,373-391`), not a
  schema constraint: `integration_id` carries no unique index
  (`crates/oc-store/src/schema.rs:192-198`).
- The primary key is the row with `active = 1`. `Store::credential_pool` orders
  the pool `active DESC, id ASC`, so the primary is tried first and the rest in
  creation order (ids encode creation time), stable across restarts
  (`crates/oc-store/src/credentials.rs:303-331`). `Store::set_credential_primary`
  moves the flag; an unknown or foreign credential id is refused without writing
  (`:333-366`).
- Cooldowns are per failing key, not per model: `oc_llm::KeyPool` keeps `quota`
  5 min, `rate_limit` 60 s, `auth` 30 min, `transport` 30 s, and never remembers
  `model_unavailable`/`invalid_request`/`empty_response`/`unknown`
  (`crates/oc-llm/src/key_pool.rs:206-217`). They are process-local and reset on
  restart; only the active flag is durable.
- Rotation is bounded per candidate: `MAX_KEY_ATTEMPTS = 8` caps how many pool
  entries one model attempt will try, even if the store reports more keys
  (`crates/oc-runtime/src/turn/stream.rs:51`, `:300`). Exhausting the pool is the
  only thing that lets `models.fallback` advance the chain, exactly once.
- Compaction cools but does not rotate: `mark_compaction_key` records the
  cooldown for the key it tried and stops there, so one compaction attempt uses
  one key and the *next* turn picks a different key
  (`crates/oc-runtime/src/turn/compaction.rs:557`). A compaction may make two
  attempts in total (one fallback hop, `COMPACTION_MAX_ATTEMPTS = 2`), each on a
  different model candidate with a single key.
- Secrets do not enter the config document as a turn credential: the runtime
  resolves the store's credential first, otherwise the provider's declared `env`
  names (`crates/oc-llm/src/client.rs:174-180`). A
  `providers.<id>.settings.apiKey` is read only by the quota probe
  (`crates/oc-server/src/data/quotas/credentials.rs:28,108`), not for turns, so a
  key belongs in the credential store or the environment.

Full design: [27](27-key-pool.md).

### `models` — routing, visibility, cost, quota

One additive, namespaced top-level object (frozen schema v1; the wire shape is
[02](02-protocol.md), the resolver contract [05](05-subagents.md), the UI
[13](13-ui.md)):

```jsonc
"models": {
  "chain":  ["google/gemini-3-pro", "opencode-go/glm-5.3-flash"],
  "chains": { "coordinator": ["google/gemini-3-pro"] },
  "fallback": { "on": ["empty_response","transport","rate_limit","quota"], "max_hops": 2 },
  "notes":  { "google/gemini-3-pro": "flat-rate tier" },
  "agent_notes": { "explore": { "opencode-go/glm-5.3-flash": "read-only worker" } },
  "hidden": ["provider/model"], "favorites": ["provider/model"],
  "pricing": { "google/gemini-3-pro": { "input":1.25,"output":5.0,"cacheRead":0.3,"cacheWrite":1.5 } },
  "quota": { "reserve_percent":10, "window_hours":5,
             "default": { "max_cost_usd":5.0, "max_requests":100 },
             "models":  { "provider/model": { "max_cost_usd":2.0, "max_requests":50 } } }
}
```

| Sub-key | Shape | Meaning |
|---|---|---|
| `chain` | `string[]` of `provider/model[#variant]` | ordered default fallback chain for the main agent; empty/absent ⇒ current single-model behavior |
| `chains` | map `agentID -> string[]` | per-role **preference order** (`engineer`, `architect`, `coordinator`, …); it selects routing order only — never an allowlist and never a visibility filter — and wins over `chain` for that role |
| `fallback.on` | `("empty_response"\|"transport"\|"rate_limit"\|"quota"\|"model_unavailable")[]` | failure kinds that advance the chain; absent ⇒ default set `[empty_response, transport, rate_limit, quota]`. `model_unavailable` is accepted here but always advances even when unlisted |
| `fallback.max_hops` | integer | maximum model switches per turn; 0 disables fallback |
| `notes` | map `provider/model -> string` | free-text note shown next to the model in the Models tab |
| `agent_notes` | map `agentID -> { provider/model -> string }` | legacy per-agent note. The Models tab does **not** edit it: it is read-only and renders as a badge ([13](13-ui.md)). The wire is not dead — `PUT /api/config/models` still accepts `agent_notes` and merges it (`crates/oc-server/src/data/models_registry.rs:1235-1236`) |
| `hidden` | `provider/model[]` | models hidden from the pickers (`visible=false` on the wire). Picker visibility is exactly `!hidden[ref] && providerConnected`; the chain and `favorites` affect **order** only ([13](13-ui.md)) |
| `favorites` | `provider/model[]` | pinned models (`favorite=true` on the wire); sorted first in the chat picker |
| `pricing` | map `provider/model -> {input,output,cacheRead,cacheWrite}` (USD per 1M tokens) | cost override applied on top of the catalog |
| `quota.reserve_percent` | number 0–100 | share of a model's window kept in reserve before the resolver treats it as exhausted |
| `quota.window_hours` | integer | rolling window (hours) the usage rollup is compared against |
| `quota.default` | `{max_cost_usd?,max_requests?}` | per-model budget applied to every model without an explicit entry |
| `quota.models` | map `provider/model -> {max_cost_usd?,max_requests?}` | per-model budget |

Rules:

- Additive and namespaced: no existing key changes meaning, and the whole block
  is absent by default. Unknown keys inside it are preserved, like every other
  config key ([§ Canonical keys](#canonical-keys) above).
- Every model ref is validated against the catalog: an unknown ref is rejected
  with `invalid_model: <ref>` and **nothing** is written (atomic). Arrays
  replace, maps merge key-by-key on write.
- Writes go through `PUT /api/config/models` only, and land in the
  highest-precedence existing global file with the same atomic write path as
  every other settings write (see "Writes" above). Per-agent quota/tone data
  lives here, not in `agents`: `agent.*` stays upstream-shaped and its
  `normalize/agents.rs` whitelist is untouched.
- Live reload: `PUT /api/config/models` stores the committed block in the
  config service's `models` cell (`ConfigService::set_models`), and the runtime
  re-parses `models` (`models_section()`) per spawn and per turn. The change
  therefore applies to the next spawn/turn with **no restart**, and the response
  carries `x-opencode-config-reloaded: models` instead of a restart hint. An
  **external edit** to the config file still needs a restart: nothing watches the
  file, and `ConfigService::refresh_models_from_disk()` runs only on demand.
- `chain`/`chains` are a **routing preference order**, never a visibility
  allowlist and not a gate: a call model (`subagent {model: …}`, `model:`
  argument) always wins even when it is outside the chain, and a model outside
  the chain is still offered by the chat picker. Picker visibility is
  `!models.hidden[ref] && providerConnected`; the chain and `favorites` only
  order the list (favorites first, then chain index, then name). The live config
  had a one-entry chain, so treating the chain as a filter hid 690 of 692 models.
- `models.chains.<role>` is the single per-role chain and `chain_for(role)` is the
  single resolver (`crates/oc-runtime/src/subagent/settings.rs:53-61`): a role
  without an entry inherits `models.chain`. Roles that resolve a model this way
  are the spawn target and the live turn's agent id
  (`crates/oc-runtime/src/subagent/spawn/prepare.rs:151`,
  `crates/oc-runtime/src/turn/stream.rs:156`) plus the standalone roles `scribe`
  (`crates/oc-cli/src/scribe.rs:164`), `observer`
  (`crates/oc-server/src/observer_host/mod.rs:158`) and `compaction`.
  `compaction` resolves through `models.chains.compaction` (else `models.chain`)
  in declared order, then falls back to the turn's current model, with **exactly
  one** fallback hop (`COMPACTION_MAX_ATTEMPTS = 2`), fewer than the turn loop's
  `models.fallback.max_hops` (`crates/oc-runtime/src/turn/compaction.rs:418-430`).
  The completed compaction row and the `session.compaction.ended` payload name the
  model that actually produced the summary — the chain candidate that answered,
  not `turn.model_ref` when the chain advanced — and `session.model` is not
  rewritten.
- `models.notes[ref]` is the **only** editable note: one inline editor on the chain
   row plus the catalog note indicator, both writing `models.notes`
   ([13](13-ui.md)). `agent_notes` stays readable for compatibility and renders a
   read-only legacy badge, and the Models tab never writes it — but the server still
   accepts and merges `agent_notes` on `PUT /api/config/models`
   (`crates/oc-server/src/data/models_registry.rs:1162-1163,1235-1236`).
- `hidden` removes a model from the chat picker (`visible=false` on the wire) and
  greys it in the Models tab list; `favorites` sorts models first in the chat
  picker. Neither changes routing.
- `pricing` is the documented cost override. The Antigravity builtin catalog
  intentionally prices every model at zero
  (`crates/oc-antigravity/src/models/builtin.rs:300-305`, overlaid by
  `crates/oc-antigravity/src/models/overlay.rs:40-57`), so a `google/*` model
  reports `cost: 0` until the user overrides it here. Builtin prices are not
  edited: existing users keep their zero-cost defaults, and the override is
  explicit and per model.

### Our additive keys

```jsonc
{
  "models":    { /* see below and 02 */ },
  "subagents": { /* see 05 */ },
  "observer":  { /* see 08 */ },
  "goal":      { /* see 08 */ },
  "memory":    { "embedding": {"model": null}, "auto_scribe": true, "max_tokens": 800 },
  "lsp_install": false,
  "lanes":     { "cargo": 1, "node": 1, "network": 4, "gpu": 1, "default": 8 },
  "events":    { "persist": true }
}
```

These are served in `GET /api/config`. `shell`, `subagents`, `observer`,
`goal`, `memory`, `lanes`, `lsp_install`, `events` and `models` are written via
`PATCH /api/experimental/config` ([02](02-protocol.md)); unknown keys in a
config document are preserved and ignored, never rejected. `models` also has a
typed writer, `PUT /api/config/models` ([02](02-protocol.md)), which validates
every ref first and answers 400 `InvalidRequestError` (`invalid_model: <ref>`)
for an unknown one, writing nothing. `PATCH /api/experimental/config` answers
`x-opencode-restart: required` (other keys are still read at startup).
`PUT /api/config/models` does **not**: it publishes the new block to the live
`models` cell and answers `x-opencode-config-reloaded: models`, so the policy
applies to the next spawn/turn. Only external edits to a config file need a
restart.

## Permissions

Rules are ordered `{action, resource, effect: allow|deny|ask}`. Evaluation is
**last match wins**; no match ⇒ `ask`. Glob matching: `*`, `?`, literal
otherwise; case-insensitive only on Windows. Rules compose:

```
agent.permissions ++ session.permissions
```

Global document `permissions` are pushed onto every agent first, then per-agent
rules are appended (agent wins). Saved rules are appended after config rules for
ask/allow resolution, but a config `deny` cannot be lifted.

Defaults:

```
*:* allow
read *.env ask
read *.env.* ask
read *.env.example allow
```

Built-in agents, canonical order: `engineer` primary (the default agent,
full tools), `architect` primary (denies `edit`/`write`/`patch` except
`docs/**`), `review` primary (denies `edit`/`write`/`patch` entirely), `general`
subagent (denies `question`; spawning is governed by `subagents.*`, not by a
permission rule), `explore` subagent (deny-all then allows
shell/grep/glob/webfetch/websearch/read/code_map/box:read (env ask) +
`external_directory`, denies `subagent`), `ui` subagent (denies
`edit`/`write`/`patch` outside `ui/**`/`web/**`), `docs` subagent (denies
`edit`/`write`/`patch` outside `docs/**`, `README.md`, `AGENTS.md`); hidden
`compaction`, `title`, `summary` deny all. Legacy ids are aliases normalized at
config load and wherever a stored id is read: `build` → `engineer`, `plan` →
`architect`, `reviewer` → `review`.
`steward` is **not** a built-in: it is a project template
(`.opencode/agent/steward.md`, `mode: subagent`) whose `permissions` and
`steps` come from the `agents.steward` entry in `opencode.json`, because agent
markdown frontmatter cannot express permissions.

Context boxes add the actions `box:read` and `box:write`, with the box key as
the resource (`docs/25-context-boxes.md`). Primaries and `general` get both;
`explore` gets `box:read` only; a custom agent can be write-only by allowing
`box:write` and denying `box:read`. `compaction.auto` defaults to `false`, so
threshold compaction only runs when the document opts in; an explicit
`compaction.ceiling_tokens` arms the same guard without `auto`, and the
threshold is `ceiling − buffer` (buffer absent ⇒ `max(8k, 10% of the window)`),
so the guard fires before the hard ceiling.

Ask flow:

1. Tool calls `permission.assert`.
2. `deny` → `BlockedError` / `permission.rejected`; `allow` → continue.
3. `ask` → insert `permission_request`, publish `permission.asked`, await.
4. Reply `once|always|reject`:
   - `once` resolves one;
   - `always` saves the `save` resources for the project and re-evaluates other
     pending requests of the same session, auto-resolving now-allowed ones;
   - `reject` fails the requester and all pending requests of that session
     (`DeclinedError`, or `CorrectedError{feedback}` when a message is given).
5. No timeout (upstream behavior). Shutdown/session close rejects all pending
   and emits `permission.replied`.

Saved permissions: `{id:"psv_…", projectID, action, resource, time}` unique per
(project, action, resource).

Policies are separate: `provider.use|tool.use|integration.use`, allow/deny only,
evaluated earliest-wins (managed policy strongest). `integration.use` matches
`mcp:<name>` / `skill:<id>` / `plugin:<target>`.

## Credentials and auth

- Credentials (OAuth/key/external) live in the SQLite `credential` table
  (`id, integration_id, label, value, active`), DB 0600. Not a keyring.
- Several rows may share one `integration_id` (backup keys); exactly one of them
  is `active`, and that invariant is enforced in the store transaction, not by
  the schema. Ordering, rotation and cooldowns: [27](27-key-pool.md). Key
  material is never written to a config document or a log line; `PoolKey`'s
  `Debug` redacts it (`crates/oc-llm/src/key_pool.rs:53-60`).
- MCP OAuth tokens use integration ids `mcp_<sha1(name+"\0"+url)[0:16]>`.
- Legacy `auth.json` is imported once.
- Server auth: password from `--password`, `OPENCODE_PASSWORD`, or
  `OPENCODE_SERVER_PASSWORD`; `serve --service` reuses `service.json:password`
  or generates a random base64url one, retrievable with `opencode service get
  password` (never printed in logs). An explicit empty password disables auth.
  Basic username is fixed `opencode`; the web app shows its connect/login
  screen when `/api/info` answers 401. Static assets and `/auth/connect` remain
  public so that screen can load.
- `credential.list` returns secret values: treat the route as privileged and
  never log bodies.

## Offline

Everything above loads locally. Well-known/Console/models.dev failures fall back
to the cached or bundled snapshot. No network on the startup path. Telemetry is
off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set; no product analytics.
