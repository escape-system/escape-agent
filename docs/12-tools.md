# 12 — Tools

Every tool is a JSON Schema function. Two rules are runtime-enforced:

1. **`expectation` is required** on every call (see [08](08-goals-observer.md)).
   The runtime injects it into the schema and strips it before dispatch.
2. **Output is truncated before it reaches the model.** The limit is part of the
   contract and surfaced in `metadata`.

Tool ids, parameter names, error strings and result metadata matter: models are
prompt-tuned to them and the web client renders by name.

## Built-in registry

Registration order (upstream): `patch, edit, glob, grep, image_generate,
opencode_* , mcp resources, question, read, schedule, shell, skill, subagent,
todowrite, webfetch, websearch, write`. Definitions are sorted by effective
name before the request; that order is part of the prompt-cache key.

Effective model name = `[^A-Za-z0-9_-] → _`, prefixed `<namespace>_` when a
namespace exists. `execute` is reserved for Code Mode.

| id | Params | Semantics | Limits |
|---|---|---|---|
| `read` | `path`, `offset?`, `limit?` | File (numbered lines) or directory listing; images/PDF as media | 2000 lines, 50 KiB, 2000 chars/line, media ≤ 20 MiB |
| `write` | `path`, `content` | Overwrite/create, parent dirs, BOM preserved, formatter run | — |
| `edit` | `path`, `oldString`, `newString`, `replaceAll?` | Three-tier replace (exact → Unicode-normalized → line-trimmed) | 0 matches/ambiguous ⇒ error |
| `patch` | `patchText` | `*** Begin Patch` envelope: add/delete/update/move, `@@` hunks | all-or-nothing per file |
| `glob` | `pattern`, `path?`, `hidden?`, `limit?` | ripgrep `--files --glob`; dotfiles hidden by default | 100 default, 30 s |
| `grep` | `pattern`, `path?`, `include?`, `literal?`, `caseSensitive?`, `limit?` | ripgrep JSON matches, excludes `.git` | 100 default, 30 s, 2000-char lines |
| `shell` | `command`, `workdir?`, `timeout?`, `background?` | Shell exec, file-backed output | 120 s foreground default; `0` = no timeout; 2000 lines/50 KiB tail |
| `todowrite` | `todos[{content,status}]` | Replace the todo list | registered only if `experimental.todo` |
| `subagent` | `agent`, `description`, `prompt`, `model?`, `sessionID?`, `background?` (+ our fields) | Child session | depth 1 upstream; ours see [05](05-subagents.md) |
| `skill` | `id` | Load `<skill_content>` + base dir + file sample | permission `skill` |
| `question` | `questions[{question,header,options[{label,description}],multiple?}]` | Asks the user via a Form | non-empty |
| `webfetch` | `url`, `format?: text\|markdown\|html`, `timeout?` | GET → text; redirects checked | 30 s default, 120 max, byte cap |
| `websearch` | `query` | Provider search | 60 s form timeout |
| `image_generate` | `prompt`, `images?[1..8]`, `model?`, `path?` | Background image job | only with an image model + `experimental.image_generate`; 20 MiB input, 30 min |
| `lsp` | `operation`, `filePath`, `line`, `character` | Definition/references/hover/symbols/call hierarchy | see [11](11-lsp.md) |
| `lsp_diagnostics` | `path` | Cached diagnostics | ours |
| `mcp_<server>_<tool>` | server schema + `expectation` | MCP call | 12 h |

Code Mode (when enabled): `execute {code}` with a JS sandbox and a catalog of
`tools.*`; `opencode_session_rename`, `opencode_session_move`,
`opencode_worktree_{create,list,remove}`, `opencode_models`,
`opencode_list_mcp_resources`, `opencode_read_mcp_resource`,
`opencode_schedule_*` (gated by `experimental.schedules`). `evaluate` is not a
tool; Code Mode is a tool transport, not an extra loop.

Our additions: `talk`, `roster`, `session_write`, `digest`, `memory_*`,
`goal_*`, `lane`, `skill_write`, `skill_lint` — see their docs.

## Input repair and validation

Before schema validation, a repair pass (max depth 6) coerces stringified
objects/arrays, `"20"→20`, `"true"→true`, and drops optional `null`/`{}`
placeholders. Then validation. On failure the model gets:

```
Invalid arguments for tool "<t>":
- <path>: <message>          (max 5 issues)

Arguments provided:
<JSON>

Update the arguments and call the tool again.
```

Never a 500, never a silent default.

## Result shape

```jsonc
{
  "output": "text for the model",
  "content": [{"type":"text","text":"…"} | {"type":"file","uri":"…","mime":"…"}],
  "metadata": {"truncated": true, "outputPath": "…", "exit": 1}
}
```

- If `content` is absent/empty, the runtime uses `{"type":"text","text":<output>}`.
- Declared output schema violations are `Tool.Error`, not a success.
- `Tool.Error {message, error?, metadata?}` → `session.tool.failed`; provider
  sees `is_error:true` and JSON `{error, content}`.
- Metadata keys the UI reads: read `{truncated}`, shell
  `{status,truncated,exit?,signal?,timeout?,shellID?}`, glob `{count,truncated}`,
  grep `{matches,truncated}`, edit/patch `{files:[{file,patch,status,additions,
  deletions}]}`, subagent `{sessionID,status}`, execute `{toolCalls,error?}`.

## Truncation

Generic: text ≤ 2000 lines and ≤ 50 KiB. If over, the full text is written to
`<data>/tool-output/tool_<id>`, the model sees the first N lines and:

```
[showing lines 1-N of M; full output saved to <path>]
```

`metadata {truncated:true, outputPath}`. If `metadata.truncated` is already set,
the generic pass is skipped. Config `tool_output {max_lines, max_bytes}`.

Shell uses the **tail** (last N lines), not the head: compiler/test errors are
at the end. MCP results cap at 1 MiB; media at 15 MiB per part.

## File tools

`read` output:

```
Read file <path>, lines A-B
<n>: <line>
```

Whole-file path only when ≤ 50 KiB and no offset/limit. Directories are listed
sorted, dirs first, `name/` suffix. Binary detection is a NUL byte in the
consumed range. `offset` is 1-based; `limit` is capped at 2000.

`edit` matching:

1. identical old/new → error;
2. empty old → error;
3. raw byte match (preserves CRLF);
4. LF-normalized exact match;
5. Unicode-normalized (quotes, dashes, spaces);
6. line-based `trimEnd()` match.

Offsets are remapped to the source; replacements use the matched region's line
endings (LCS alignment). 0 matches and >1-without-`replaceAll` are errors with
the upstream wording.

`patch` grammar: `*** Begin Patch` … `*** Add File: p` / `*** Delete File: p` /
`*** Update File: p` (+`*** Move to:`) / `*** End Patch`, `@@` context hunks.
All files are locked before apply; a partial failure reports what completed.

Per-model selection: if the model id contains `gpt-`, is not `oss`, and is not
`gpt-4`, expose `patch` and hide `edit`/`write`; otherwise the reverse.

## Shell

- Parse with `shell-words`; if the command is a simple argv form, execute
  directly; otherwise use the configured shell with `-c`.
- Shell selection: `bash|sh|zsh|ksh` (login) on Unix; `pwsh → powershell →
  git-bash → COMSPEC` on Windows. fish/nu are denied.
- `workdir` must be a directory inside the location unless
  `external_directory` is allowed.
- Env: session env, then `TERM=xterm-256color`, `OPENCODE_TERMINAL=1`,
  `AGENT=1`, `OPENCODE=1`, `AI_AGENT||=opencode`, `OPENCODE_SESSION_ID`.
- No PTY by default; `background:true` returns a shell id and streams
  `session.shell.*`; foreground streams chunks and publishes progress every 2 s.
- Cancel: `SIGTERM`, then `SIGKILL` after 3 s; process group/Job Object.
- File-backed output is capped as above; the full file is referenced in
  metadata and usable by `digest`.

## Subagent output

Foreground completes with:

```jsonc
{"sessionID":"ses_…","status":"completed","output":"final text only"}
```

Model content:

```
<subagent sessionID="ses_…" state="completed">
final text
</subagent>
```

Background returns `status:"running"` and a do-not-poll instruction. The parent
never receives the child's tool log; `digest` is the pull path.

## Dynamic descriptions

Descriptions are static per tool except:

- `shell` rewrites its description with the selected shell and OS;
- `subagent` appends the available subagent roster;
- `websearch` removes itself when disabled;
- `patch` toggles `edit`/`write`.

These are computed per request from the step snapshot. A plugin transform that
changes the tool set triggers `instructions.updated` and a new snapshot; an
in-flight request is never mutated.

## Permission actions

`read, edit, glob, grep, shell, subagent, skill, webfetch, websearch,
todowrite, question, image_generate, external_directory, list_mcp_resources,
read_mcp_resource, lsp, memory:write, goal, lane, skill:write`, plus the full
MCP tool name. V1 aliases (`bash→shell`, `task→subagent`,
`write|patch→edit`) are accepted in config and never emitted.

Every file tool asserts `external_directory` before its own action when the
path escapes the location.

## Build lanes (ours)

`shell` classifies build/test commands (`cargo`, `npm/pnpm/bun/yarn`, `go`,
`pytest`, `dotnet`, `gradle`, `mvn`) and routes them through `LaneService`.
The tool call returns a ticket immediately; progress and output stream as
events. The coordinator agent uses the `lane` tool to order, cancel and notify.
Safety does not depend on the model: the lane semaphore is enforced in Rust.
See [05](05-subagents.md).
