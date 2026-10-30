# 07 — Skills and commands

Byte-compatible with upstream v2 skill discovery, plus agent-authored skills.
A skill is **instructions**, never executable code.

## On-disk format

```
<root>/skill/<id>/SKILL.md        (preferred)
<root>/skill/<name>.md            (loose file; id = basename)
<root>/skills/<id>/SKILL.md       (plural root, same rules)
```

```markdown
---
name: Rust Build & Test
description: Serialize cargo build/test through lanes; never run raw cargo test.
metadata:
  opencode/autoinvoke: true
---
Markdown body is the skill content...
```

Frontmatter: only `name?`, `description?`, `metadata?`,
`disable-model-invocation?` are read; unknown keys are ignored (upstream
behavior). `autoinvoke = metadata["opencode/autoinvoke"] ??
(disable-model-invocation ? false : undefined)`; booleans accept
`true/yes/on/1` and `false/no/off/0`. ID = directory name, or file basename for
a loose `<name>.md`. `name` defaults to the id.

## Discovery and precedence

Config roots, in load order, each contributing `skill/` then `skills/`:

```
managed (system + MDM) → integrations → well-known → global →
explicit config → direct files → project .opencode (outermost → innermost) → inline
```

Then the `skills[]` config array (relative to the location directory; `~/…`
expands; `http(s)://` pulls a bundle), highest precedence last. Compatibility
roots are scanned too: `~/.claude/skills`, `~/.agents/skills`, and project
`.claude/skills`, `.agents/skills` (only the `skills` subdir).

Same id later wins. Scan is recursive, depth ≤ 6, skips dot-entries and
`node_modules/.git/.venv/venv/__pycache__`; a directory with `SKILL.md` stops
recursion. Results sorted by id. A 100 ms debounced filesystem watcher triggers
a full rescan and emits `skill.updated`.

URL bundles: `GET <url>/index.json` → `{"skills":[{"name","version?","files":[
"SKILL.md",…]}]}`; must contain `SKILL.md` or `<name>.md`; cached under
`<cache>/skills/<hash(url)>/<name>/` with atomic refresh.

## Advertising

The index is a system instruction source (`core/skill-guidance`), not a tool
description:

```
Skills provide specialized instructions and workflows for specific tasks.
Use the skill tool to load a skill when a task matches its description.
...
<available_skills>
  <skill id="rust-build-test" name="Rust Build &amp; Test" description="…"/>
</available_skills>
```

Filtered by `skill:<id>` permission (deny excluded), `description` present,
`autoinvoke !== false`; sorted by id. Loading a skill returns its body as
`<skill_content id="…" name="…">…</skill_content>` and publishes
`session.skill.activated`.

## Self-authored skills

Tool `skill_write` (permission `skill:write`, default `ask`):

```jsonc
{
  "id": "kebab-case-id",
  "name": "Human Name",
  "description": "when to use this (≤ 200 chars)",
  "body": "markdown",
  "scope": "project|global",
  "autoinvoke": true
}
```

- Path is forced to `<scope>/.opencode/skill/<id>/SKILL.md` (project) or
  `<config>/skill/<id>/SKILL.md` (global). `id` is `^[a-z0-9][a-z0-9-]{1,63}$`.
- Frontmatter is generated; the body is not interpreted. Atomic write
  (temp + rename), then the watcher reloads.
- Marked `source:"agent"` in the `skill.updated` payload and the UI.
- `skill_lint {path|id}` validates frontmatter, length, description, and
  duplicate id; the tool refuses to overwrite a non-agent skill unless
  `overwrite: true` and the permission is `allow`.
- Skills cannot bundle executables; any `scripts/` folder is ignored by the
  index and never auto-run. A skill can *instruct* the agent to run a script,
  which then goes through the normal shell permission.

The agent is prompted to write a skill only after a repeatable workflow has
succeeded twice, and to keep bodies under 200 lines.

## Commands (slash commands)

Same discovery roots, `command/` and `commands/`:

```markdown
---
description: Run the focused Rust test lane
agent: build
model: anthropic/claude-sonnet-4-5
subtask: true
---
Run `lane.queue` for: $ARGUMENTS
```

- `name` = file basename; `$ARGUMENTS`, `$1..$9` template substitution; body
  becomes the user prompt; `agent`/`model`/`subtask` override the turn.
- Built-in commands ship as embedded markdown (same mechanism, not code).
- `GET /api/command` returns the catalog; `POST /api/session/:id/command`
  executes one.
- A command may reference a skill; the skill content is injected as a
  `<skill_content>` block before the command body.

## Built-in skills we ship

| id | Purpose |
|---|---|
| `rust-build-test` | Big-brother build/test protocol: lanes, ordering, nextest, target dirs |
| `report` | Diagnostics snapshot (upstream-compatible) |
| `memory-hygiene` | When to write/forget memory; dedupe rules |
| `observer-repair` | How an agent should react to an observer repair note |

`rust-build-test` is also installed at `opencode-rust/.opencode/skill/` so our
own agents use it while porting.

## Compatibility notes

- Upstream drops unknown frontmatter keys; we preserve them in the file but do
  not read them, and the UI shows a warning.
- Upstream's `skills[]` URL bundles require network at config load; we fetch in
  the background and serve the cached copy in the meantime (no boot stall).
- We do not execute skill code, and we never load a skill from a path outside
  the configured roots, even if the model asks.
