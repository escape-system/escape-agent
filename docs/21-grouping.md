# 21 — Agent grouping (`metadata.group`)

Status: **design decided, convention documented, nothing implemented.** This
document adds no code path. Section [What is deliberately not
implemented](#what-is-deliberately-not-implemented) is the binding part: the
representation below is the only grouping concept that exists, and it is a
label, not a mechanism.

## Problem

The session tree is a single-parent forest (`session_v2.parent_id`). That is
enough for one coordinator with a star of workers, and it is what
`docs/05-subagents.md` describes, but it cannot express a **cross-coordinator
wave**: several coordinators, each with its own children, that belong to one
logical unit of work.

Three concrete consequences:

1. `roster` (`crates/oc-runtime/src/subagent/roster.rs:32-81`) returns exactly
   `parent`, `siblings` and `children` (built at `:70-79`). A worker has no way
   to see, or even name, a worker under a *different* coordinator.
2. Coordinator messaging scope is derived from the parent subtree:
   `in_coordinator_scope` (`crates/oc-subagents/src/message.rs:105-118`) takes
   the coordinator's **parent** and then `has_ancestor(target, parent)`. So a
   coordinator can reach its own siblings and their descendants and nothing
   else. Two coordinators in the same wave whose parents differ can never
   message each other, however they are labelled.
3. There is no set-membership concept anywhere on the wire. `grep -c group`
   is `0` in both `ui/vendor/opencode/packages/client/src/promise/generated/types.ts`
   and `ui/vendor/opencode/packages/protocol/openapi.json`; upstream has no
   grouping idea at all.

## Chosen representation

`metadata.group` — an optional **non-empty string** on the existing open
`Session.Metadata` object.

Exact shape on `Session.Info`:

```jsonc
{
  "id": "ses_…",
  "projectID": "prj_…",
  "cost": 0,
  "tokens": { "input": 0, "output": 0, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
  "time": { "created": 1762300000000, "updated": 1762300000000 },
  "location": { "directory": "/work/repo" },
  "metadata": { "group": "wave-7" }        // optional; absent when unset
}
```

Rules:

- **Optional.** When no group is set, the key is absent — not `null`, not `""`
  (AGENTS.md optional-field invariant; `docs/README.md:43`).
- **Non-empty string only.** `""`, whitespace-only, and non-string values are
  invalid. Readers treat an empty string as absent rather than as a group;
  the existing metadata readers already do this for `subagentMode`
  (`crates/oc-runtime/src/subagent/settings.rs:156-163` trims and filters
  empty). No server-side validation is added by this document.
- **Descriptive label only.** It has no permission, policy, routing, budget or
  lifecycle effect. It does not widen `message_decision`
  (`crates/oc-subagents/src/message.rs:120`) or `write_decision` (`:176`); it
  is not consulted by `Policy`, `Limits`, or `roster`.
- **Not globally unique.** Two unrelated waves may reuse `"wave-7"`. The label
  is a human/tooling convenience, not an identity.

Why `metadata` and not a new field: `Session.Metadata` is the **only open
object on the wire**. `types.ts:65` declares
`SessionMetadata = { [x: string]: JsonValue }`; `SessionInfo.metadata` is
`SessionMetadata` (`types.ts:2238`, block `2225-2242`); OpenAPI defines
`Session.Metadata` as `{ "type": "object" }`
(`openapi.json:22808-22809`), while the enclosing `Session.Info`
(`openapi.json:21841`) closes at `21926-21927` with
`"additionalProperties": false`. Any *new named* field would therefore be an
illegal extension of a closed schema; an arbitrary key inside `metadata` is
already legal for every client.

### Precedent

The repo already uses exactly this pattern, and this document follows it
rather than inventing one:

| Key | Written | Read |
|---|---|---|
| `metadata.subagentMode` | `crates/oc-server/src/data/session/prompt.rs:76` | `crates/oc-runtime/src/subagent/settings.rs:157` (`pointer("/metadata/subagentMode")`) |
| `metadata.maxDepth` | `crates/oc-server/src/data/session/prompt.rs:77` | no reader today (write-only) |
| `metadata.environment` | `crates/oc-server/src/data/session_ext/view.rs:37-51` (read-modify-write, merges) | run environment |
| `metadata.lane` | `crates/oc-runtime/src/subagent/spawn.rs:203` | — |

`metadata.lane` is the cautionary example: `spawn.rs:203` builds
`lane.map(|lane| json!({ "lane": lane }))`, which **replaces** the whole
metadata object instead of merging. A `group` must not be written that way —
see the PATCH caveat below.

## Usage

Because `metadata` is already accepted everywhere a session is created or
updated, no new route is needed.

Create with a group (`POST /api/session`; `metadata` is accepted at
`crates/oc-server/src/data/session/bodies.rs:39` and passed through at
`crates/oc-server/src/data/session/crud.rs:108`; the wire body is documented at
`docs/02-protocol.md:88`):

```jsonc
POST /api/session
{ "title": "wave 7 coordinator A", "metadata": { "group": "wave-7" } }
```

Set or change it after creation (`PATCH /api/session/:SID`; body field at
`bodies.rs:52-61`, applied in `crud.rs:176-186`, which also publishes durable
`session.metadata.updated` with the new object; `docs/02-protocol.md:91`):

```jsonc
PATCH /api/session/ses_…
{ "metadata": { "group": "wave-7", "subagentMode": "complex" } }
```

> **PATCH replaces the whole metadata object.** `crud.rs:176-186` assigns
> `patch.metadata` wholesale, and the store writes `metadata = ?` directly
> (`crates/oc-store/src/session.rs:547-550`). To add a group to a session that
> already has metadata keys, read `GET /api/session/:SID` first and send the
> merged object, exactly as the environment route does
> (`crates/oc-server/src/data/session_ext/view.rs:37-51`).

### Spawning: PATCH after spawn

The `subagent` tool schema (`crates/oc-runtime/src/subagent/spawn.rs:79-103`)
has no `group` property, and its `"additionalProperties": false` (`:101`)
means a call that passes one is rejected. The documented flow is therefore:

1. spawn the child as usual (`subagent` with `description`/`prompt`, plus
   `role`, `background`, `spawn`, `lane` as needed);
2. `PATCH /api/session/<child id>` with `metadata.group`, merging any existing
   keys.

A `group` **tool parameter** (not a wire field) added to the TOOL schema would
make this a single call and remains wire-safe, because it would travel only in
the model's tool arguments and be written into `metadata` by the host. That is
a future improvement; it is not implemented and this document does not assume
it.

### Unchanged semantics

- `roster` still returns ids/agents/statuses/titles for parent, siblings and
  children only (`roster.rs:32-81`; documented shape in
  `docs/05-subagents.md:160-166`). Groups are not listed.
- `talk` still takes a single `to` (`crates/oc-runtime/src/subagent/talk.rs:35`,
  read at `:48`); `session_write` still takes a single `session_id`
  (`crates/oc-runtime/src/subagent/write.rs:47`). There is no broadcast.
- Authorization is untouched: `message_decision`
  (`crates/oc-subagents/src/message.rs:120`) and `write_decision` (`:176`) do
  not read metadata.
- The group survives fork (`crates/oc-server/src/data/session_ext/fork.rs:88`
  copies `metadata`), import (`crates/oc-server/src/data/session_ext/import.rs:109`),
  and appears in `session.created` (`crates/oc-server/src/data/session/events.rs:68-80`
  propagates `metadata`).

## Rejected alternatives

### A new `session_v2` column `group_id`

Rejected. `session_v2` is declared at `crates/oc-store/src/schema.rs:39-75`
with the five indexes at `:76-80` (`project_id`, `directory`, `parent_id`,
`(time_updated, id)`, `(time_archived, time_updated)`); there is no index on
`root_id` either. A column is not a wire change on its own, but:

- it must be added to `SESSION_COLUMNS` (`crates/oc-store/src/session.rs:122-127`)
  and threaded through `session_value` (`:716-813`) to be visible at all —
  and today `session_value` deliberately never reads positional indices `3-7`
  (`root_id`, `depth`, `role`, `spawn_allowed`, `child_count`), which is
  exactly why those columns stay off the wire;
- once emitted it would be a **new field on `Session.Info`**, whose schema is
  closed (`openapi.json:21926-21927`) and whose generated type
  (`types.ts:2225-2242`) does not contain it. That breaks the wire-compat
  invariant in AGENTS.md;
- adding a migration means a new `Migration` entry (`schema.rs:406-456`)
  executed by the checksummed runner (`crates/oc-store/src/db.rs:403-448`;
  refusal at `:421-423`, `column_guard` at `:426-435`, `column_exists` at
  `:456-463`). That is real cost for zero wire benefit.

### A new `session_group` table

Rejected. It would be invisible on the wire by construction, so no client could
ever use it; it needs invented membership semantics (can a session be in two
groups? who removes a row when a session is deleted?); it participates in
deletion via `delete_tree` (`crates/oc-store/src/session.rs:874-910`), which
deletes session rows and relies on FK `ON DELETE CASCADE` — a new table would
need its own cascade or it would leak rows; and it would have to be taught to
fork (`fork.rs:88`) and import (`import.rs:109`), both of which copy `metadata`
for free today. Membership is precisely the thing `metadata.group` does *not*
claim to provide.

### `role: "coordinator"` as the grouping mechanism

Rejected. `role` is a per-session policy value, not set membership: it is one
of `["primary","subagent","coordinator"]`
(`crates/oc-store/src/session.rs:695-705`), it is stored in `session_v2`
(`schema.rs:43`) but **not on the wire** (`session_value` skips index 5), and
it gates messaging scope through the parent subtree
(`crates/oc-subagents/src/message.rs:105-118`). Two sessions with
`role=coordinator` have no relationship to each other. `docs/05-subagents.md:21`
already says `role` is a policy hint, not a boundary — it is certainly not a
group.

### `lane`

Rejected. `lane` is a **shell-command concurrency scheduler**:
`LaneService` (`crates/oc-lanes/src/service.rs:91`, doc comment `:85-90`)
classifies cargo/test commands (`classify`, `crates/oc-lanes/src/classifier.rs:28`)
and serializes them with a per-lane semaphore. It says nothing about sessions.
Its persistence is dead in any case: `lane_ticket` (`schema.rs:275-287`) is
written only by `save_lane_ticket` (`crates/oc-store/src/lane.rs:47`), which
has **zero production callers** — only `oc-store` tests call it
(`crates/oc-store/tests/store_ext/lanes.rs`), as `docs/17-interfaces.md:217-219`
already notes. `metadata.lane` on a spawned child (`spawn.rs:203`) is a hint to
that scheduler, not a set.

### `parentID` alone

Rejected as sufficient. A coordinator plus children is one star; a wave of `n`
coordinators is `n` stars with no edge between them. `parentID` also has no
expressive room to grow: `Session.Info.parentID?` is a single optional id
(`types.ts:2227`), and `session.list` filters on exactly one `parentID`
(`openapi.json:919-1068`, fourth parameter). Labelling the participating
sessions is the minimum change that makes the wave discoverable by a human or
an external tool, which is all this document claims.

## Why there is no server-side `group` filter

Adding `?group=` to `GET /api/session` is the obvious next wish and is
explicitly **not** part of this design.

- `openapi.json` operation `session.list` (`operationId` at `:918`) enumerates
  exactly nine query parameters in `:919-1068` — `limit`, `order`, `search`,
  `parentID`, `archived`, `directory`, `project`, `subpath`, `cursor` — and the
  array closes at `:1068` with no `additionalProperties: true` anywhere in the
  operation. The generated `SessionListInput` (`types.ts:3180-3190`) lists the
  same nine. A server-only parameter therefore **cannot be expressed through
  the generated client**.
- `docs/02-protocol.md:384-385`: *"All new APIs live under
  `/api/experimental/*` … new event types are additive."* A filter bolted onto
  an upstream route is neither.

If filtering is ever needed before it is expressible, the client-visible
option is `search` over titles, or fetching `GET /api/session` pages and
reading `metadata.group` client-side. A proper filter belongs in an
`/api/experimental/*` route with its own schema, and would need its own design
note.

Related pre-existing gap, recorded here so it is not rediscovered: upstream
accepts a `subpath` query parameter on `session.list`
(`openapi.json`, eighth parameter; `SessionListInput.subpath`,
`types.ts:3188`), but the server's `ListParams`
(`crates/oc-server/src/data/session/bodies.rs:10-20`) has no such field, so
passing it is silently ignored. That is a server gap against upstream, not a
grouping decision.

## What is deliberately not implemented

| Not implemented | Why |
|---|---|
| Server-side `?group=` filter on `session.list` | Not expressible in `SessionListInput`/`openapi.json`; upstream route is closed to new params; `docs/02-protocol.md:384-385` |
| Group broadcast (`talk`/`session_write` to a set) | Both tools take a single target (`talk.rs:35`, `write.rs:47`); fan-out would need new authorization semantics and is not in `Policy` |
| `roster` listing group peers | Cross-tree visibility is a permission change, not a label; `message_decision` would still refuse delivery, so the listing would be a lie |
| Durable `session_group` table / membership | Invented semantics, invisible on wire, extra cascade work in `delete_tree` (`session.rs:874-910`), not copied by fork/import |
| Group permissions, budgets, spawn limits | `metadata.group` is descriptive; `role`/`spawn_allowed`/`depth` remain the only policy inputs (`schema.rs:39-75`, `docs/05-subagents.md:21`) |
| New `Session.Info` field or `session.group.updated` event | `Session.Info` is closed (`openapi.json:21926-21927`); PATCH already publishes `session.metadata.updated` (`crud.rs:176-186`) |
| `group` parameter on the `subagent` tool | Wire-safe future improvement; the tool schema is `additionalProperties: false` today (`spawn.rs:101`), so the flow is PATCH after spawn |

No `todo!()`, no stub, no route answers success for any of the rows above.
