# 02 — HTTP protocol

Normative sources, in priority order:

1. `../opencode/packages/client/src/promise/generated/types.ts` (exact JSON).
2. `../opencode/packages/protocol/openapi.json` (paths, statuses, schemas).
3. This document (explanations and Rust-specific decisions).

Where this document and 1–2 disagree, fix this document. This file describes the
**landed** server, not a wish list: a route is listed only if it is registered,
and anything that answers an honest `501`/`503` says so in its row.

## Transport and bootstrap

- Plain HTTP/1.1 + HTTP/2, no TLS. Default `127.0.0.1:4096`; if busy, scan
  4096→65535. `--hostname`, `--port`, `--cors`.
- Bind the listener first (fail fast, port scan), but start `axum::serve` only
  after the store, config, catalog, runtime and extensions are attached, so
  `/api/info` is always 200 once the port accepts connections. There is no
  `service_starting` 503 path in the current build.
- `GET /api/info` →
  `{version,pid,hostname?,urls[],paths:{tmp},capabilities?{persistentPty}}`.
- Host guard (all routes, public and protected): a request carrying a `Host`
  header must name this server. Loopback names (`localhost`, `*.localhost`,
  loopback IPs) and the configured `--hostname` are accepted; binding
  `0.0.0.0`/`::` (unspecified) is permissive so LAN hostnames work. Anything
  else → `403 ForbiddenError` `{"message":"Host not allowed"}`. A request with
  no `Host` header passes. This blocks DNS rebinding.
- Password sources (first non-empty wins): `--password`, `OPENCODE_PASSWORD`,
  `OPENCODE_SERVER_PASSWORD`; an explicit empty value disables auth. `serve
  --service` reuses the password already in its `service.json` and otherwise
  generates one; `opencode service get password` prints it (also `url`, `pid`,
  `id`, `version`). The static app shell (`/`, `/_assets`, `sw.js`, icons) and
  `/auth/connect/:code` stay public so the vendored connect/login screen can
  load; every `/api` route is protected.
- Auth (only when a password is set): `Authorization: Basic base64("opencode:"+pw)`;
  the Basic password position also accepts the 30-day HMAC **session token**
  (`opencode:<token>`); `?auth_token=<token>` accepts either the session token
  or the legacy `base64("opencode:"+pw)` pair; cookie `opencode_session_<port>`
  is accepted only for a same-origin request. Username is fixed `opencode`. 401
  body `{"_tag":"UnauthorizedError","message":"Authentication required"}`;
  send `WWW-Authenticate` only when `sec-fetch-mode` is absent or `navigate`.
- Pairing: `POST /api/pair` → `{code,expires_in:300}` (single-use, 5 min). It is
  on the **protected** router, so with a password set it requires Basic or a
  session token (with no password it is open like every route). `GET
  /auth/connect/:code` is public (skips Basic, still subject to the Host guard)
  → JSON `{token}` where `token` is the HMAC session token (expiry + signature,
  30 days; empty string only when no password is configured), or (HTML accept)
  `302 /` + httpOnly SameSite=Lax cookie (when a password is set) valid 30
  days. Ticket-guarded WebSocket paths still bypass Basic auth.
- Location resolution: `?location[directory]=<abs>` (deepObject) →
  header `x-opencode-directory` (URI-decoded) → process cwd. Session-scoped
  routes use the session's location, not the request header. `GET /api/location`
  answers the bare `Location.PublicInfo` (`{directory, project:{id,directory,
  canonical}}`) and a missing directory is `404 LocationNotFoundError`; the
  location envelope (`L`) is still used by every list route.
- Envelopes:
  - `L` = `{location:{directory,project:{id,directory,canonical}},data:<X>}`
  - `D` = `{data:<X>}` (the generated client types these routes as `{data}`)
  - `B` = bare JSON (array or object)
  - `N` = 204 empty
  - `S` = SSE
- Optional fields are **omitted**, never `null`. Timestamps are epoch ms
  integers. IDs are prefixed strings (`ses_`, `msg_`, `evt_`, `req_`, `per_`,
  `psv_`, `obs_`, `gol_`, `mem_`, `lan_`, `frm_`, `cred_`, `wkt_`), 26 base62
  chars after the prefix.
- Query booleans are literal `true|false`; numbers decimal; arrays are
  repeated keys (`?types=a&types=b`); `parentID=null` is the literal string
  `"null"`.
- Body limit (upstream has none; deliberate): one 64 MiB `DefaultBodyLimit` for
  the router; `POST /api/experimental/fs/write` enforces the same 64 MiB on its
  raw body. Exceeding → 413 with `RequestTooLargeError`. There is no separate
  prompt/command text cap.

## Core route table

`SID` = `:sid`. `Env` is the envelope column above.

| Method + path | Query / body | Env | Notes |
|---|---|---|---|
| GET `/api/info` | — | B | health; 200 once serving |
| POST `/api/pair` | — | B | protected when a password is set; `{code,expires_in}` |
| GET `/auth/connect/:code` | — | B | public; `{token}` = session token, or 302+cookie |
| GET `/api/location` | loc | B | bare `Location.PublicInfo`; 404 `LocationNotFoundError`; upserts the `LocationRef` project row when a store is attached |
| POST `/api/location/reload` | — | N | rescans skills only; 503 without config (no full config reload) |
| GET `/api/agent` / `/api/agent/:id` | loc | L | `Agent.Info[]` / `Agent.Info`; 404 |
| GET `/api/session` | `limit?,order?,search?,parentID?,archived?,directory?,project?,cursor?` | D | `{data,cursor}` |
| POST `/api/session` | `{id?,parentID?,title?,agent?,model?,location?,metadata?,permissions?}` | D | 404 `LocationNotFoundError` |
| GET `/api/session/active` | — | D | `Record<sid,{type:"running"}>` |
| GET `/api/session/:SID` | — | D | 404 |
| PATCH `/api/session/:SID` | `{title?,metadata?,permissions?,archived?}` | N | emits durable `session.renamed/metadata.updated/permissions/archived/unarchived` |
| DELETE `/api/session/:SID` | — | N | recursive child delete; durable `session.deleted` first |
| POST `/api/session/:SID/fork` | `{before?}` | D | copies messages below the boundary; durable `session.forked` v2 on the child |
| POST `/api/session/:SID/agent` | `{agent}` | N | 404 |
| POST `/api/session/:SID/model` | `{model}` | N | 404 |
| POST `/api/session/:SID/move` | `{directory,delivery?}` | N | 400 |
| POST `/api/session/:SID/prompt` | `{id?,text,files?,agents?,skills?,metadata,delivery?,resume?}` | D | 409 duplicate id; a text of exactly `/compress` (case-insensitive) admits a compaction row instead of a user message |
| POST `/api/session/:SID/command` | `{name,text?,files?,agents?,skills?,delivery?}` | N (204) | 404 `CommandNotFoundError`, 400 empty; renders `$ARGUMENTS`; builtin `compress` (alias of `/compact`) admits a compaction row unless a markdown `compress` command exists |
| POST `/api/session/:SID/synthetic` | `{id?,text,description?,metadata,delivery?,resume?}` | D | `SessionInbox.Synthetic` |
| POST `/api/session/:SID/shell` | `{id?,command}` | N | starts a background managed shell bound to the session |
| POST `/api/session/:SID/compact` | `{id?,delivery?}` | D | `SessionInbox.Compaction` |
| POST `/api/session/:SID/revert/stage` | `{messageID,files?}` | D | `{messageID,files:FileDiff.Info[]}`; diffs the earliest pre-write snapshot of each path at/after the boundary against the current tree; 404 unknown message |
| DELETE `/api/session/:SID/revert` | — | N | restores every earliest snapshot (rewrites the pre-image, or deletes a file that did not exist), then drops the snapshots |
| POST `/api/session/:SID/revert/commit` | — | N | deletes boundary seq onward, drops snapshots, clears revert |
| GET `/api/session/:SID/context` | — | D | bare `{"data":[...]}` of the wire messages AFTER the newest compaction row whose status is absent or `"completed"`; the boundary row itself is excluded; no token/window/ceiling fields |
| GET `/api/session/:SID/diff` | `from?,to?,context?` | D | `FileDiff.Info[]`; recomputed from snapshots vs the current tree for a staged revert; `from`/`to` are only validated as message ids (they do not select a historical range) and `context` is accepted but every hunk is full-file |
| GET `/api/session/:SID/inbox` | — | D | pending items |
| PATCH `/api/session/:SID/inbox/:id` | `{delivery}` | N | 404 `NotFoundError`, 400 invalid delivery |
| DELETE `/api/session/:SID/inbox/:id` | — | N | no-op if delivered |
| PUT `/api/session/:SID/environment` | `{variables}` | N | stores `metadata.environment`; non-string values 400 |
| POST `/api/session/:SID/view` | `{idle:epochMs}` | N | `time_viewed` + durable `session.viewed` v1 |
| POST `/api/session/:SID/interrupt` | `resume?` | B | `{interrupted:bool}` (false with no runtime) |
| POST `/api/session/:SID/background` | — | N | wakes the actor; 503 without runtime |
| GET `/api/session/:SID/message` | `limit≤200,order?,cursor?,type?` | D | default 50, desc |
| GET `/api/session/:SID/message/:messageID` | — | D | 404 `MessageNotFoundError` |
| GET `/api/session/:SID/form` | — | D | `Form.Info[]` (pending only; settled forms are fetched by id) |
| POST `/api/session/:SID/form` | `{id?,title,metadata?,fields}` | D | 409 duplicate id; 400 field validation |
| GET `/api/session/:SID/form/:formID` | — | D | 404 `FormNotFoundError` |
| POST `/api/session/:SID/form/:formID/reply` | `{answer}` | N | 404; 409 `FormAlreadySettledError`; 400 `FormInvalidAnswerError` |
| DELETE `/api/session/:SID/form/:formID` | `message?` | N | 404; 409 when settled |
| GET `/api/session/:SID/permission` | — | D | pending broker requests; 503 without broker |
| POST `/api/session/:SID/permission` | `{id?,action,resources,save?,metadata?,source?,agent?}` | D | `{id,effect}`; `ask` enqueues a broker request |
| GET `/api/session/:SID/permission/:requestID` | — | D | 404 `PermissionNotFoundError` |
| POST `/api/session/:SID/permission/:requestID/reply` | `{decision,message?}` | N | 404 |
| GET `/api/permission/request` | loc | L | pending requests across the location |
| GET `/api/permission/saved` | `projectID?` | D | saved rules |
| PATCH `/api/permission/saved/:id` | `{action?,resource?}` | D | 404 `PermissionSavedNotFoundError` |
| DELETE `/api/permission/saved/:id` | — | N | 204 even when unknown |
| GET `/api/experimental/session/:SID/export` | `sanitize?` | D | `{info,messages}`; `sanitize=true` drops secret keys |
| POST `/api/experimental/session/import` | `{info,messages,location?}` | D | 409 duplicate, 404 location |
| GET `/api/experimental/session/stats` | `from,to,project,timezone,tools` | D | full scan |
| POST `/api/experimental/session/:SID/wait` | `timeout?` (s) | N | 204 when idle, 503 timeout; 404 session |
| POST `/api/experimental/session/:SID/skill` | `{id,resume?}` | N | 404 `SkillNotFoundError`; enqueues the skill body as a synthetic item (the runtime `skill` tool publishes durable `session.skill.activated` v1) |
| POST `/api/session/:SID/generate` | `{prompt}` | D | 503: no generator attached |
| GET `/api/experimental/session/:SID/instructions/entries` | — | D | instruction sources |
| PUT/DELETE `/api/experimental/session/:SID/instructions/entries/:key` | `{value}` | N | 413 above 1 MiB; the runtime emits durable `session.instructions.updated` v2 when the prefix changes |
| GET `/api/session/:SID/log` | `after?`, `follow?` | S | durable replay (alias of the experimental path) |
| GET `/api/experimental/session/:SID/log` | `after?`, `follow?` | S | durable replay then live, `log.synced` |

`GET /api/session/:SID/context`, `diff`, `export`, `stats`, `import`,
`instructions`, `skill` and `wait` are our surface; the generated client does
not call them.

File snapshots: before a `write`, `edit` or `patch` tool call executes, the
runtime captures the current bytes of every target that resolves inside the
session directory. Each captured file is capped at 5 MiB; a target that does
not exist is recorded as absent (so revert deletes it), and files that are
unreadable, irregular (not regular files), or over the cap are recorded as
skipped (the tool still runs, the revert/diff simply leaves that path alone).
Paths outside the session directory are never captured. `POST
/revert/stage` ignores its `files` flag (the diff is always computed) and
`DELETE /revert` restores the earliest captured pre-image per path, removing
files that did not exist at capture time; skipped paths stay untouched.

### Model / provider / tools

| Method + path | Query / body | Env |
|---|---|---|
| GET `/api/model`, `/api/model/default` | loc, `types?` / loc | L | only available providers: configured in `providers`, backed by a stored credential, or with a declared env var set; disabled providers excluded |
| GET `/api/provider`, `/api/provider/:id` | loc | L | same availability filter as `/api/model` |
| GET `/api/integration`, `/api/integration/:id` | loc | L | every non-disabled catalog provider, connectable via a `key` method plus an `env` method when env names are declared |
| POST `/api/integration/:id/connect/key` | `{key,answer?,label?}` | N (204) |
| POST `/api/integration/:id/connect/external` | `{methodID,answer?,label?}` | N (204) |
| POST `/api/integration/:id/connect/oauth` | `{methodID}` | — (400 when the catalog method is absent; 501 only for a registered oauth method, unreachable with today's catalog) |
| GET/DELETE `/api/integration/:id/connect/oauth/:attemptID` | — | — (404 `IntegrationAttemptNotFoundError` / 204) |
| POST `/api/integration/:id/connect/oauth/:attemptID/complete` | `{code?}` | — (404) |
| POST `/api/integration/:id/connect/command` | `{methodID,label?}` | — (400 when the catalog method is absent; 501 only for a registered command method, unreachable with today's catalog) |
| GET/DELETE `/api/integration/:id/connect/command/:attemptID` | — | — (404 / 204) |
| POST `/api/experimental/integration/wellknown` | `{url}` | — (503 discovery not configured) |
| GET `/api/credential` | — | D |
| POST `/api/credential` | `Credential.CreateInput` | D | 409 duplicate id |
| GET `/api/credential/:id` | — | D | extra route, not in OpenAPI; 404 |
| PATCH `/api/credential/:id` | `{label}` | N | unknown id is a no-op |
| DELETE `/api/credential/:id` | — | N | unknown id is a no-op |
| POST `/api/credential/:id/activate` | — | N | unknown id is a no-op |
| GET `/api/websearch/provider` | loc | L | truthful empty list (no provider) |
| GET `/api/experimental/usage/limits` | loc | L | signed out: `data` is `null` |
| GET `/api/command`, `/api/skill`, `/api/reference` | loc | L | `/api/command` appends the builtin `compress` (`{name,description}` only) when no markdown command shadows it |
| GET `/api/config` | — | B | `Config.Entry[]`; process snapshot, ignores `location` |
| GET `/api/config/shell` | — | B | `ConfigShell.Option[]` |
| PATCH `/api/experimental/config` | allowlisted keys only | N | 400 unknown key; `x-opencode-restart: required` |
| POST `/api/experimental/generate/{text,image,decisions}` | body | — | 503 after body validation (no generator service) |
| POST `/api/experimental/generate` (deprecated alias) | body | — | 503 |

`POST /api/websearch` is **not registered**; callers get the JSON 404
`NotFoundError` rather than a fake search result.

Credential injection: the runtime resolves a provider's key from the SQLite
`credential` table at request time, preferring the integration's `active` row
and otherwise the first matching credential (tagged `key` or an OAuth access
token). If none exists, the LLM client falls back to the provider's declared
environment variables. A connect via `/api/integration/:id/connect/key` or a
`POST /api/credential` takes effect without a restart. The observer uses the
same resolution per judge call; a declared-but-unset `provider.env` no longer
disables it, and a judge call with no usable credential fails open by
publishing `observer.unavailable`.

### Files, VCS, shells, terminals

| Method + path | Query / body | Env |
|---|---|---|
| GET `/api/fs/read/*` | loc | B | bytes, ETag/Range |
| GET `/api/fs/list` | loc, `path?` | L | directory picker: resolves absolute/relative paths without location confinement (parents, other drives); `fs/read`/`fs/find` stay confined. Entries extend upstream `{path,type}` with `name, absolute, size, modified` (JSON clients ignore unknown fields): `path` is relative to the requested location (also for a nested `path=`), falling back to the absolute text outside the location; `absolute` is always the resolved path; symlinks report their resolved `file`/`directory` kind |
| GET `/api/fs/find` | loc, `query,type,limit?` | L | same entry shape; `path` relative to the location |
| POST `/api/experimental/fs/write` | loc, `path`, raw bytes | L | 64 MiB cap, atomic rename |
| POST/DELETE `/api/experimental/fs/watch` | loc, `{path}` | N | polling watcher; publishes ephemeral `filesystem.changed` |
| GET `/api/vcs`, `/base`, `/status`, `/branch`, `/diff` | loc + mode/base/context | L |
| POST `/api/vcs/init` | loc, `provider?` | N |
| GET `/api/worktree` | `projectID` (required; upstream OpenAPI) | B | `Worktree.Directory[]` (bare array) |
| POST `/api/worktree` | `Worktree.CreateInput` | B | `Worktree.Info` `{directory}`; real `git worktree add` |
| DELETE `/api/worktree` | `{projectID,directory?,force?}` | N | one or all of the project's worktrees |
| POST `/api/worktree/refresh` | `{projectID}` | N | `git worktree prune` + reconcile |
| GET/POST `/api/shell`, GET/DELETE `/api/shell/:id`, GET `/api/shell/:id/output` | loc | L |
| GET/POST/PUT/DELETE `/api/pty*`, POST `/connect-token` | loc; `x-opencode-ticket: 1` | L |
| GET/POST/PUT/DELETE `/api/experimental/persistent-pty*` | same, plus `GET /snapshot`, `POST /connect-token` | D (WS via ticket) |
| GET/POST `/api/experimental/session/:SID/terminal`, GET `/read` | `lines?` | D |
| GET `/api/experimental/media/:hash` | — | — (404: no blob store) |
| POST `/api/experimental/client-mcp/token` | — | — (503: tunnel absent) |
| GET `/api/pty/:id/connect` | WS, `ticket` | — |
| GET `/api/experimental/persistent-pty/:id/connect` | WS, ticket/cursor/… | — |

`GET /api/experimental/client-mcp` (WebSocket) is **not registered**.
`POST /api/experimental/client-mcp/token` answers `503` instead of minting an
unusable ticket.

### MCP, LSP, projects, events, misc

| Method + path | Query / body | Env |
|---|---|---|
| GET `/api/mcp`, `/api/mcp/resource` | loc | L | 503 without manager |
| PUT/DELETE `/api/experimental/mcp/:server` | loc, `{config}` / — | N | validates config; publishes `mcp.status.changed` |
| POST `/api/experimental/mcp/:server/{connect,disconnect}` | loc | N | 502 connect failure; 404 unknown server |
| GET `/api/lsp` | loc | L | status; never forces a spawn |
| GET `/api/lsp/diagnostics?path=` | loc | B | raw diagnostics array; 404 when unknown/empty |
| POST `/api/lsp/reload` | — | N | restarts clients, clears diagnostics |
| GET `/api/project`; PATCH `/api/project/:id` | `archived?` | B |
| GET `/api/form` | loc | L | pending forms across sessions (settled via `GET /api/session/:SID/form/:formID`) |
| GET `/api/plugin` | loc | L | inventory |
| GET `/api/plugin/gui` | loc, `id` | — | always 404 (no GUI bundles loaded) |
| POST `/api/plugin/check` | `{target?}` | L | inventory, no invented `outdated` flags |
| POST `/api/plugin/update` | `{targets[]}` | N | empty → 204; otherwise 503 (no installer) |
| POST `/api/rpc/:rpcID/:method` | loc, `{input?}` | B | `{}` or `{output}`; 404/400/500/503 |
| GET `/api/event` | `types?` repeated | S |
| GET `/api/debug/location`; DELETE | loc | B / — | DELETE answers 501 (no cache) |
| POST `/api/debug/heap-dump` | — | — | 501 |
| GET `/api/experimental/migration/v1` | — | — | 501 (reads no v1 data) |
| GET `/api/experimental/observer` | `sessionID` | D | audit, budget, repairs; 400 missing |
| POST `/api/experimental/observer/check` | `{sessionID}` | N | forced pass; 400 missing sessionID, 503 without runtime/observer |
| GET `/api/experimental/observer/repairs` | `sessionID`, `limit?` | D | durable repairs |
| GET `/api/experimental/goal` | `sessionID` | D | root-resolved; 400 missing, 409 one active goal |
| POST `/api/experimental/goal` | `{sessionID,objective,criteria?,model?}` | D | 409 one active goal |
| PATCH `/api/experimental/goal/:id` | `{status?,rounds?,stalls?,lastProgress?,evidence?}` | D | 409 reactivation |
| GET `/api/experimental/goal/history` | `sessionID` | D |
| GET/POST `/api/experimental/memory`, DELETE `/api/experimental/memory/:id` | scoped query / body | D |
| GET `/api/experimental/subagent/tree?sessionID=` | — | D | `{root,nodes}` |
| POST `/api/experimental/subagent/:SID/message` | `{text,delivery?}` | D |
| GET `/api/experimental/lane` | `lane?` | D | 503 without lane service |
| POST `/api/experimental/lane/:id/order` | `{priority}` | N |
| POST `/api/experimental/lane/:id/cancel` | — | N |
| GET `/api/experimental/schedule` | `sessionID?` | D | list; 404 unknown session |
| POST `/api/experimental/schedule` | `{sessionID,name?,when,ends?,prompt}` | D | 404 session; 400 bad trigger/prompt |
| GET `/api/experimental/schedule/:id` | — | D | 404 `ScheduleNotFoundError` |
| PATCH `/api/experimental/schedule/:id` | `{name?,when?,ends?,prompt?}` | D | 404; 400 |
| DELETE `/api/experimental/schedule/:id` | — | N | 404 |
| POST `/api/experimental/schedule/:id/pause` | — | D | 404 |
| POST `/api/experimental/schedule/:id/resume` | — | D | 404; repeating moves to its next future slot, a one-shot keeps its due instant |

`when` is exactly one trigger plus an optional `tz`:
`{"type":"in","in":"5 minutes"}`, `{"type":"at","at":"2026-10-27T09:00:00Z"}`,
`{"type":"every","every":"2 hours"}` or `{"type":"cron","cron":"*/5 * * * *"}`.
Durations accept a plain integer (seconds) or `<n> <unit>` pairs (`ms`, seconds,
minutes, hours, days, weeks), `every` must be ≥ 1 minute, `at` must be in the
future, and cron is five-field (`minute hour day month weekday`). Only `UTC` and
numeric `+HH:MM`/`-HH:MM` zones are evaluated; an IANA name (e.g.
`Europe/Berlin`) is rejected with 400 rather than treated as UTC. `ends` is
`{after?, at?, times?}` and stops the schedule at the first limit reached.
The runner fires due rows once per second (batch 16), records `outcome` as
`fired`/`late` (≥ 60 s behind)/`failed`, and admits the prompt with the stable
inbox id `msg_<scheduleID>_<run>` so a crash replays as a conflict rather than a
duplicate.

## SSE (`GET /api/event`)

- Headers: `content-type: text/event-stream`, `cache-control: no-cache, no-transform`,
  `x-accel-buffering: no`, `x-content-type-options: nosniff`.
- Frame: `data: <json>\n\n`. Heartbeat: `: heartbeat\n\n` every 15 s.
- First frame **must** be `{"id":"evt_…","type":"server.connected","data":{}}`
  with no `created` and no `durable` key, even when `?types=` filters
  everything else.
- `?types=` is repeatable and comma-separated (`?types=a&types=b,c`); the
  filter applies to live events only and is applied after the mandatory
  `server.connected` frame.
- Per-subscriber queue 4096, dropping. When a subscriber falls behind the
  broadcast channel reports a lag; the server **ends that stream** (the
  connection closes). No error frame is written and no events are replayed. The
  contract is "close, never a silent gap"; clients resync from REST. There is
  no `Last-Event-ID` support aside from the session log below.
- Never buffer or compress SSE; never send a BOM.

Event envelope:

```jsonc
{
  "id": "evt_…",
  "created": 1730000000000,
  "type": "session.text.ended",
  "location": {"directory": "/abs"},        // when location-scoped
  "metadata": {},                          // optional
  "durable": {"aggregateID": "ses_…", "seq": 42, "version": 1}  // durable only
}
```

`data` is the type-specific payload. Delta events (`session.text.delta`,
`session.reasoning.delta`, `session.tool.input.delta`, `session.tool.progress`,
`session.usage.updated`, `session.status`,
`session.idle`, `form.created`, `form.replied`, `form.cancelled`) are
ephemeral: no `durable` marker and no seq reservation. All `*.ended`, tool
called/success/failed, step/execution events, `session.viewed`,
`session.forked`, `session.skill.activated`, `session.instructions.updated`
and the compaction lifecycle (`session.compaction.started`,
`session.compaction.ended`, `session.compaction.failed`; all v1, published
after the corresponding commit), `session.execution.step_limit` (v1,
`{sessionID, steps}`, always after `session.execution.succeeded`) and the
host-side `observer.advised` (v1, `{sessionID, estimateTokens, ceilingTokens,
triggerTokens}`) are durable. `session.compaction.delta` is not implemented.

`GET /api/experimental/session/:SID/log?after=<seq>&follow=true` (also exposed
as `GET /api/session/:SID/log`) replays durable events for that session
(`seq > after`) then streams live, ending the replay with `log.synced`. This is
the only resumable stream.

## Errors

Effect tagged errors serialize as JSON with `_tag` and the declared status:

```json
{"_tag":"SessionNotFoundError","message":"Session not found: ses_…","sessionID":"ses_…"}
```

Statuses: 400 `BadRequestError`/`SchemaError`/`InvalidRequestError`, 401
`UnauthorizedError`, 403 `ForbiddenError` (Host guard or policy), 404 per entity
(`SessionNotFoundError`, `MessageNotFoundError`, `FormNotFoundError`,
`LocationNotFoundError`, `ProjectNotFoundError`, `IntegrationNotFoundError`,
`PermissionNotFoundError`, `PermissionSavedNotFoundError`, …), 409
`ConflictError`/`FormAlreadySettledError`/duplicate inbox id, 413
`RequestTooLargeError`/`InstructionEntryValueTooLargeError`, 500
`InternalError`, 502 `ProviderError`, 503 `ServiceUnavailableError` (a
manager/service is not attached), 501 `NotImplementedError` (registered route
whose engine genuinely does not exist).

Rules: never return a 200 with an error body; never leak a Rust panic message;
map unknown internal errors to `{"_tag":"InternalError","message":"Internal error"}`
and log the correlation id.

## Compatibility gotchas (each has a test)

1. The generated client returns the response JSON **as-is**; it does not strip
   `{data}`. A route whose OpenAPI schema is `{data:…}` must send that envelope,
   and a route whose schema is a bare array/object must send it bare (for
   example `/api/worktree`).
2. 204 responses must have no body and no `content-type: application/json`.
3. `message.updated` / `message.part.*` must **not** be emitted (v1-only).
4. `server.connected` has no `created` and no `durable`.
5. `durable.seq` comes from one per-session counter shared by `event`,
   `session_message` and `session_inbox`; message rows use that same seq. The
   event log therefore has gaps (messages and inbox rows consume numbers it
   never shows, and delivered inbox rows are deleted). Do not invent a
   separate message counter and never assume contiguity.
6. Event id → message id: strip `evt_`, add `msg_`. Client-side dedupe depends
   on it.
7. Cursors are base64url JSON, not opaque IDs; the client sends back what it got.
8. `parentID=null` is the literal string, not an absent param.
9. `Location.response` must echo the requested directory and its project.
10. `GET /api/fs/read` supports `Range`, `If-None-Match` (304), `Last-Modified`.
11. Media refs (`media:<sha256hex>`) are **not** served: there is no blob store,
    so `/api/experimental/media/:hash` is always 404. Media returned by tools is
    inline base64/data URL.
12. WebSocket upgrades bypass header auth; they require a single-use ticket
    minted by `connect-token` with `x-opencode-ticket: 1`. They are still
    subject to the `Host` guard (403 `ForbiddenError`).
13. Integration OAuth/command engines are absent: connect attempts answer `501`
    and attempt lookups `404`; only `key`/`external` connect writes credentials.

## Our extensions (no upstream compat promise)

All new APIs live under `/api/experimental/*` (plus the named routes above) and
new event types are additive. The web client ignores unknown events and fields.

Durable extension events: `session.forked` (v2, data
`{sessionID,parentID,boundary,instructions?,instructionEntries?}`),
`session.viewed` (v1, `{sessionID,idle}`), `session.skill.activated` (v1, from
the runtime skill tool), `session.instructions.updated` (v2, from the runtime
instruction prefix), `session.execution.step_limit` (v1, `{sessionID,steps}`,
always after `session.execution.succeeded`, never a terminal itself) and
`observer.advised` (v1, `{sessionID,estimateTokens,ceilingTokens,
triggerTokens}`, from the host-side compaction adviser; its payload is
runtime-local, not declared in `oc-types`).

Ephemeral extension events: `form.created` (`{form:Form.Info}`),
`form.replied` (`{id,sessionID,answer}`), `form.cancelled` (`{id,sessionID}`),
`observer.repaired`, `observer.escalated`, `observer.unavailable`,
`goal.updated`, `lane.queued|started|finished|cancelled`, `schedule.fired`
(`{scheduleID,sessionID,outcome}`),
`subagent.peer.message`, `lsp.client.diagnostics`, `lsp.updated`,
`mcp.status.changed`, `filesystem.changed`, `pty.*`, `persistent-pty.*`,
`shell.created|deleted|exited`, plus the runtime's session deltas.

New config is served through `GET /api/config` documents and written through
`PATCH /api/experimental/config` (allowlisted keys: `shell`, `subagents`,
`observer`, `goal`, `memory`, `lanes`, `lsp_install`, `events`), never through
ad-hoc per-feature writes. See [14](14-config-permissions.md).

## Static web app

- Same origin. `/api*`, `/auth/*`, `/openapi.json` never fall back to HTML (JSON
  404).
- `/` and every non-asset GET/HEAD path → `index.html`, 200, `no-cache`.
- `_assets/**`, `icons/**`: 200 with `public, max-age=31536000, immutable`;
  missing → 404 `no-store`.
- `sw.js`, `registerSW.js` → `no-cache`.
- Static responses are compressed when the client negotiates it: gzip/brotli
  (fastest level), only for bodies ≥ 1 KiB, with `vary: accept-encoding` on
  every static response. SSE (`/api/event`, log streams) is never compressed:
  the compression layer wraps only the static fallback, not `/api`.
- `*.map` is never served (404 `no-store`).
- Non-GET/HEAD on a non-API path → 405 with `allow: GET, HEAD`.
- The Rust server sets `content-type`, `cache-control` and
  `x-content-type-options: nosniff`. It does **not** inject a
  `Content-Security-Policy`; the app's `_headers` file is only used by static
  hosts.
- Assets are read from disk (`OC_WEB_DIST`, else `web/dist`); a missing build
  answers 503 with a build hint.
