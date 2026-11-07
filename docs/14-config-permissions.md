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
| `default_agent` | string | `build` |
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
| `compaction` | `{auto?,keep?{tokens?},buffer?}` | true / 15000 / 15% |
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

### Our additive keys

```jsonc
{
  "subagents": { /* see 05 */ },
  "observer":  { /* see 08 */ },
  "goal":      { /* see 08 */ },
  "memory":    { "embedding": {"model": null}, "auto_scribe": true, "max_tokens": 800 },
  "lsp_install": false,
  "lanes":     { "cargo": 1, "node": 1, "network": 4, "gpu": 1, "default": 8 },
  "events":    { "persist": true }
}
```

These are served in `GET /api/config` and written via
`PATCH /api/experimental/config`. Unknown keys in a config document are
preserved and ignored, never rejected.

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

Built-in agents: `build` primary + `question allow`; `general` subagent, denies
`question` and `subagent`; `explore` deny-all then allows
shell/grep/glob/webfetch/websearch/read/code_map (env ask) +
`external_directory`, denies `subagent`; `plan` denies edits except
`~/.opencode/plan/*`; hidden `compaction`, `title`, `summary` deny all.

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
