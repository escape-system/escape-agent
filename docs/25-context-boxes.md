# 25 — Context boxes

Context boxes are durable, root-shared stores of working context. They exist
because compaction used to *replace* history with the newest summary: each
generation re-summarized the previous summary, facts eroded, and long sessions
turned into a telephone game. Boxes are the stable substrate a summary can
point at instead of restating.

They are not `oc-memory` (doc 06): memory is scribe-driven, capped at 2,000
characters, scoped global/project/agent/session and retrieved by FTS. Boxes are
explicitly authored by the agent, larger, addressed by key, lifecycle-tagged and
shared by the whole session tree.

## Compaction policy

Compaction runs only when asked:

- the user (`/compact`, `/compress`, `POST /api/session/:sid/compact`), or
- the main agent through the `compact_context` tool, or
- the observer repair `compact_context`, or
- the provider rejects a request for context overflow (recovery, not policy).

`compaction.auto` gates the token-threshold path and defaults to **`false`** in
this deployment. Setting it to `true` restores threshold compaction for hosts
that want it. `compaction.ceiling_tokens` optionally lowers the computed ceiling
(`context_window − max(15%, 16k)`); it can never raise it. The old hard-coded
150k cap is gone, so a 1M-window model compacts at its real ceiling.

The summary request now carries the current box index and is instructed to
reference box keys instead of restating stable content.

## Data model

```sql
CREATE TABLE context_box (
  id            TEXT PRIMARY KEY,          -- box_…
  root_id       TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  owner_session TEXT NOT NULL,             -- session that last wrote it
  key           TEXT NOT NULL,             -- [a-z0-9._-]{1,64}
  title         TEXT,
  description   TEXT,
  kind          TEXT NOT NULL,             -- stable | temporary
  tags          TEXT NOT NULL DEFAULT '[]',
  content       TEXT NOT NULL,
  revision      INTEGER NOT NULL DEFAULT 1,
  time_created  INTEGER NOT NULL,
  time_updated  INTEGER NOT NULL,
  time_expired  INTEGER,
  deleted       INTEGER NOT NULL DEFAULT 0,
  UNIQUE(root_id, key)
);
```

Scope is the session tree: every session whose `session_v2.root_id` is the
same root sees the same boxes. A subagent writes to the tree it belongs to, not
to a private copy. A box is addressed by `key` within its root, so two boxes in
one tree cannot share a key.

Wire shape (`id`, `rootID`, `key`, `title`, `description`, `kind`, `tags`,
`revision`, `size` = content characters, `timeCreated`, `timeUpdated`,
`timeExpired`). Optional fields are omitted, never `null`.

Limits: store rejects content above 262,144 characters; `box_write` caps at
60,000; `box_read` returns at most 20,000 characters per call with
`offset`/`limit` chunking. The index lists at most 100 boxes.

## Kinds and compaction

| Kind | Survives compaction | Visible in the index |
|---|---|---|
| `stable` | yes | yes |
| `temporary` | no (gets `timeExpired`) | no, unless `include_expired` |

A successful compaction stamps `time_expired` on every live `temporary` box of
the root, after the `completed` compaction row is committed and before
`session.compaction.ended` is published. Stable boxes are untouched. Expiry is
soft: the row and its content remain queryable with `include_expired` for
audit, but `box_read`/`box_list` and the prompt index ignore it by default. The
next compaction does not re-expire rows.

`stable` is not a promise of correctness; it is the author's statement that the
fact must outlive the summary. The index is the index of record: when the model
needs the content it calls `box_read`.

## Prompt integration

The instruction prefix gains `oc/boxes`, ordered after `oc/memory` and before
`oc/goal`:

```
<boxes root="ses_…">
- [stable] decisions (12 chars) — architecture decisions that must survive
- [temporary] scratch (80 chars)
</boxes>
```

Only key, kind, size and the description/title are rendered; content stays out
of the prefix. The source is pinned for the lifetime of the actor epoch, exactly
like `oc/goal`: a box created or updated mid-epoch is delivered as a tail
supplement so the cached prefix does not churn. A new actor epoch (restart,
compaction does not reset it) renders the fresh index into the prefix.

## Tools

| Tool | Action | Resource | Behaviour |
|---|---|---|---|
| `box_list` | `box:read` | `*` | Index of live boxes; optional `kind`, `include_expired` |
| `box_read` | `box:read` | box key | Chunked content with size notice |
| `box_write` | `box:write` | box key | Create/update/append; `kind` defaults to `stable` |
| `box_delete` | `box:write` | box key | Soft delete |
| `compact_context` | `compaction` | `*` | Enqueue a `manual` compaction for the next boundary |

Every call is authorized by the runtime before dispatch, like any other tool.
`box_read` on an expired box fails unless `include_expired` is set; `box_write`
on an expired key revives it (`time_expired` cleared, revision bumped).

Append is `box_write` with `append: true`: the new text joins the existing
content with a newline and the same cap applies.

## Permissions

Actions are `box:read` and `box:write`; the resource is the box key, so
wildcard rules give per-box and per-agent control:

```json
{"permissions": [
  {"action": "box:read",  "resource": "*",        "effect": "allow"},
  {"action": "box:write", "resource": "scratch*", "effect": "deny"}
]}
```

Built-in defaults:

| Agent | `box:read` | `box:write` |
|---|---|---|
| `build` (and other primaries) | allow | allow |
| `general` | allow | allow |
| `explore` | allow | deny (`deny("*","*")`) |
| `plan` | allow | allow |

A write-only reporter is a custom agent with `box:write` allowed and
`box:read` denied; it can call `box_write` but not `box_read`/`box_list`. The
main agent can always write.

## Failure and durability

- Box rows are written in the store's writer thread in one transaction.
  Revision and timestamps are allocated there, so ordering survives kill -9.
- `box_write` returns the committed row; it never reports success before the
  commit.
- Compaction expiry is not part of the terminal event: a crash between the
  `completed` row and expiry leaves temporary boxes live. That is safe (they
  are context, not effects) and the next successful compaction expires them.
- Boxes are not copied by `fork`/`import` today. A fork starts with the same
  root tree relation only if the fork keeps the root; a new root starts empty.
