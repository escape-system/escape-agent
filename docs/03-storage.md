# 03 — Storage

SQLite, one file, WAL. The schema mirrors upstream v2 where the wire depends on
it; new tables are additive. The goal is byte-level compatibility with the
upstream `opencode.db` for sessions/messages/parts, and a clean, smaller
internal model for our features.

## Engine and file

- `rusqlite` with `bundled` (no system SQLite on Windows), one writer
  connection owned by a dedicated writer thread, and a fixed read pool of four
  connections behind mutexes (reads run on `spawn_blocking`; no `r2d2`).
- Pragmas: `journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`,
  `cache_size=-32768` (32 MiB; upstream uses 64 MiB — we cut it for the RSS
  budget), `foreign_keys=ON`, `temp_store=MEMORY`.
- File: `$OPENCODE_DB`, else `<data>/opencode.db` where `<data>` is
  `$OPENCODE_DATA_DIR`, `$XDG_DATA_HOME/opencode`, or
  `$HOME/.local/share/opencode` (temp dir on Windows without `HOME`). Sidecars
  `-wal`, `-shm`. Only the database and its sidecars are created today;
  `log/`, `snapshot/`, `repos/`, `tool-output/` and `<state>/locks` are planned,
  not implemented.
- Migration journal: `migration(id TEXT PRIMARY KEY, time_completed INTEGER,
  checksum TEXT)`. Migrations are embedded `&str`, run in order, each in its
  own transaction, checksum verified. A checksum mismatch refuses to start
  (never silently skip). Current ids: `0001_wire`, `0002_additive`,
  `0003_memory_fts`, `0004_project_archived`, `0005_form`,
  `0006_lane_ticket_message`, `0007_lane_ticket_output_path`,
  `0008_lane_ticket_queued_idx`, `0009_schedule`, `0010_snapshot`.

## Where sessions are stored

Everything durable lives in **one SQLite database** (WAL):

- Sessions, messages and parts: `session_v2`, `session_message`,
  `session_part`; pending input in `session_inbox`; the durable log in
  `event`/`event_sequence`.
- The same file also holds projects, memory, goals/observations/repairs,
  forms, schedules, snapshots, credentials, worktrees and lane tickets.

File resolution, first match wins:

1. `opencode serve --db <path>` (or `$OPENCODE_DB`).
2. `$OPENCODE_DATA_DIR/opencode.db`.
3. `$XDG_DATA_HOME/opencode/opencode.db`.
4. `$HOME/.local/share/opencode/opencode.db`.
5. `%TEMP%/opencode/opencode.db` (the code checks `HOME`, not `USERPROFILE`, so
   Windows without `HOME` lands in temp).

WAL sidecars are `<db>-wal` and `<db>-shm`. The DB is never inside the repo
unless `--db` points there; the running test server uses
`opencode-rust/.test-server/opencode.db` (ignored by `*.db`). `Store::open_memory`
is for tests only. Browser/UI state is not authoritative — the server DB is.

Deleting a session cascades its messages, parts, inbox rows and event rows;
projects and memory are shared rows and survive. Back up while the server is
stopped (copy the DB plus both sidecars) or use SQLite's backup API; never copy
only the main file while WAL is active.

## Core schema (wire-compatible)

```sql
session_v2(
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  parent_id TEXT, root_id TEXT, depth INTEGER NOT NULL DEFAULT 0,
  role TEXT NOT NULL DEFAULT 'primary',          -- primary|subagent|coordinator
  spawn_allowed INTEGER NOT NULL DEFAULT 1,
  child_count INTEGER NOT NULL DEFAULT 0,
  fork_session_id TEXT, fork_boundary TEXT,
  directory TEXT, path TEXT,
  metadata TEXT, permission TEXT, model TEXT, revert TEXT, summary_diffs TEXT,
  cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0,
  tokens_output INTEGER NOT NULL DEFAULT 0, tokens_reasoning INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read INTEGER NOT NULL DEFAULT 0, tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
  time_busy INTEGER, time_idle INTEGER, time_viewed INTEGER,
  time_idle_outcome INTEGER, time_compacting INTEGER, time_archived INTEGER,
  time_suspended INTEGER, resume_attempts INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX session_project_idx  ON session_v2(project_id);
CREATE INDEX session_workspace_idx ON session_v2(directory);
CREATE INDEX session_parent_idx   ON session_v2(parent_id);
CREATE INDEX session_time_idx     ON session_v2(time_updated DESC, id DESC);

session_message(
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type TEXT NOT NULL, seq INTEGER NOT NULL, data TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);
CREATE INDEX message_session_type_seq ON session_message(session_id, type, seq);
CREATE INDEX message_session_time     ON session_message(session_id, time_created, id);

session_part(
  message_id TEXT NOT NULL REFERENCES session_message(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type TEXT NOT NULL, call_id TEXT, data TEXT NOT NULL,
  PRIMARY KEY(message_id, ordinal)
);
CREATE INDEX part_session_message ON session_part(session_id, message_id, ordinal);

session_inbox(
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type TEXT NOT NULL,                 -- user|synthetic|compaction|move|peer|observer
  payload TEXT NOT NULL,
  delivery TEXT NOT NULL,             -- steer|queue
  enqueued_seq INTEGER NOT NULL,
  UNIQUE(session_id, enqueued_seq)
);
CREATE INDEX inbox_delivery ON session_inbox(session_id, delivery, enqueued_seq);

event_sequence(aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);

event(
  id TEXT PRIMARY KEY,
  aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL, created INTEGER NOT NULL,
  type TEXT NOT NULL,                 -- "<type>.<version>"
  data TEXT NOT NULL,
  UNIQUE(aggregate_id, seq)
);
CREATE INDEX event_aggregate_type_seq ON event(aggregate_id, type, seq);

blob(hash TEXT PRIMARY KEY, mime TEXT, bytes INTEGER, width INTEGER, height INTEGER,
     data BLOB NOT NULL, time_created INTEGER NOT NULL);
blob_ref(session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE,
         message_id TEXT, hash TEXT REFERENCES blob(hash) ON DELETE CASCADE,
         seq INTEGER, PRIMARY KEY(session_id,message_id,hash));

instruction_entry(session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE,
                  key TEXT, value TEXT, removed INTEGER NOT NULL DEFAULT 0,
                  PRIMARY KEY(session_id,key));
instruction_state(session_id TEXT PRIMARY KEY REFERENCES session_v2(id) ON DELETE CASCADE,
                  epoch_start INTEGER, through_seq INTEGER,
                  initial_values TEXT, current_values TEXT);

permission_request(id TEXT PRIMARY KEY, session_id TEXT, action TEXT, resources TEXT,
                   save TEXT, metadata TEXT, source TEXT, message TEXT,
                   time_created INTEGER NOT NULL);
permission_saved(id TEXT PRIMARY KEY, project_id TEXT, action TEXT, resource TEXT,
                 time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                 UNIQUE(project_id, action, resource));

project(id TEXT PRIMARY KEY, directory TEXT NOT NULL, canonical TEXT NOT NULL,
        name TEXT, icon TEXT, time_created INTEGER, time_updated INTEGER);
worktree(id TEXT PRIMARY KEY,
         project_id TEXT REFERENCES project(id) ON DELETE CASCADE,
         directory TEXT NOT NULL, canonical TEXT, branch TEXT, name TEXT,
         time_created INTEGER, time_updated INTEGER);
credential(id TEXT PRIMARY KEY, integration_id TEXT, label TEXT,
           value TEXT, active INTEGER);
kv(key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

Notes on the wire tables that carry more columns than their wire object:

- `worktree` stores `canonical`, `branch`, `name` and timestamps internally, but
  the HTTP/`Store` surface only ever emits `Worktree.Info = {directory}`.
  `(project_id, directory)` is unique in practice: `create_worktree` refuses a
  duplicate directory for the same project with `Conflict`. Ids are `wkt_…`.
- `credential` rows are `Credential.Entry` on the wire
  (`{id, integrationID, label, active, value}`). `value` is opaque JSON tagged
  `{"type": "oauth" | "key" | "external", …}`. At most one row per integration
  is `active`; `create_credential`/`update_credential`/`activate_credential`
  clear the sibling flags in the same transaction. Ids are `cred_…`.
  Legacy rows without `integration_id` are treated as absent by
  `get_credential` and skipped by `list_credentials`.
- `project.archived` and the `project_archived_idx`/`project_time_idx` indices
  are added by migration `0004` behind a column guard.

`session_message.data` and `session_part.data` keep the upstream JSON key
layout exactly (`$.status`, `$.model.providerID`, `$.tokens.*`, `$.snapshot.*`,
`$.time.completed`, `$.shellID`, `$.text`). Compaction, stats, revert, export
and the UI use `json_extract` on those keys; renaming a Rust field must not
rename a JSON key.

## Additive schema (our features)

```sql
goal(id TEXT PRIMARY KEY, root_id TEXT NOT NULL, objective TEXT NOT NULL,
     criteria TEXT NOT NULL, status TEXT NOT NULL, rounds INTEGER DEFAULT 0,
     stalls INTEGER DEFAULT 0, last_progress INTEGER, evidence TEXT,
     model TEXT, time_created INTEGER, time_updated INTEGER);
CREATE UNIQUE INDEX goal_active_root ON goal(root_id) WHERE status='active';

observation(id TEXT PRIMARY KEY, root_id TEXT, session_id TEXT, tool_call_id TEXT,
            tool TEXT, why TEXT, expect_s INTEGER, intervene_s INTEGER,
            started INTEGER, settled INTEGER, state TEXT, outcome TEXT,
            parent_call TEXT, synthetic INTEGER DEFAULT 0);
CREATE INDEX observation_root_active ON observation(root_id, state, started);

observer_repair(id TEXT PRIMARY KEY, root_id TEXT, target_id TEXT, kind TEXT,
                reason TEXT, args_hash TEXT, attempt INTEGER, time_created INTEGER);

memory(id TEXT PRIMARY KEY, scope TEXT NOT NULL, scope_id TEXT, kind TEXT NOT NULL,
       text TEXT NOT NULL, tags TEXT NOT NULL DEFAULT '[]', importance INTEGER,
       confidence REAL, source TEXT, source_session TEXT, source_message TEXT,
       hash TEXT NOT NULL, created INTEGER, updated INTEGER, last_used INTEGER,
       uses INTEGER DEFAULT 0, deleted INTEGER DEFAULT 0,
       UNIQUE(scope, scope_id, hash));
CREATE VIRTUAL TABLE memory_fts USING fts5(text, tags, content='memory', content_rowid='rowid');

lane_ticket(id TEXT PRIMARY KEY, lane TEXT NOT NULL, command TEXT NOT NULL,
            cwd TEXT, priority INTEGER, owner_session TEXT, state TEXT,
            queued INTEGER, started INTEGER, finished INTEGER, exit_code INTEGER,
            message TEXT,                 -- failure/exit message (migration 0006)
            output_path TEXT);            -- combined stdout/stderr spool (0007)
CREATE INDEX lane_ticket_queued_idx ON lane_ticket(queued DESC, id DESC);

form(id TEXT PRIMARY KEY,
     session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
     title TEXT NOT NULL, metadata TEXT, fields TEXT NOT NULL,
     status TEXT NOT NULL DEFAULT 'pending'
       CHECK (status IN ('pending', 'replied', 'cancelled')),
     reply TEXT, cancel_message TEXT,
     time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
CREATE INDEX form_session_status_time ON form(session_id, status, time_created DESC, id DESC);
CREATE INDEX form_status_time        ON form(status, time_created DESC, id DESC);

schedule(id TEXT PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
         name TEXT, when_json TEXT NOT NULL, tz TEXT NOT NULL,
         action_json TEXT NOT NULL,
         status TEXT NOT NULL DEFAULT 'active'
           CHECK (status IN ('active', 'paused', 'done')),
         next_due_at INTEGER, ends_at INTEGER, remaining_runs INTEGER,
         run_count INTEGER NOT NULL DEFAULT 0,
         last_fired_at INTEGER, last_outcome TEXT,
         time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
CREATE INDEX schedule_status_due_idx ON schedule(status, next_due_at);
CREATE INDEX schedule_session_idx    ON schedule(session_id);

snapshot(id TEXT PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
         message_id TEXT NOT NULL,
         seq INTEGER NOT NULL,
         tool_call_id TEXT NOT NULL,
         path TEXT NOT NULL,
         existed INTEGER NOT NULL,
         skipped INTEGER NOT NULL,
         content BLOB,
         time_created INTEGER NOT NULL);
CREATE INDEX snapshot_session_seq_path ON snapshot(session_id, seq, path);
```

`golden` rule: additive tables never share names with upstream tables. Import
from an upstream DB copies only the tables in the wire-compatible section.

Forms are ours, not upstream: upstream keeps them in memory with a ten-minute
TTL, while we persist them so a pending question survives a restart. `fields`
is the `Form.Field[]` JSON, `metadata` an optional object, `reply` the stored
`{answer}` object, `status` the settled state. Ids are `frm_…`. `GET
/api/form` reads this table across sessions; the session routes read it by
`session_id`. `session.instructions.updated` and `session.skill.activated` are
durable v2/v1 events, but they do not get their own tables: instruction entries
live in `instruction_entry`/`instruction_state`, skill activation is a durable
event only.

Schedules store the wire `Schedule.When` and `Schedule.Action` objects verbatim
in `when_json`/`action_json`; `tz` is the resolved zone (`UTC` or a numeric
`+HH:MM`/`-HH:MM`; IANA names are rejected at write time). `next_due_at`,
`ends_at` and `remaining_runs` are precomputed from `when`/`ends` so the runner
is a single indexed `status='active' AND next_due_at <= now` query. Status is
`active|paused|done`; `run_count` counts attempts, `last_outcome` is one of
`fired|late|skipped|disabled|failed`. Ids are `sch_…`.

Snapshots are captured by the runtime (`oc-runtime::snapshot`) immediately
before a `write`, `edit` or `patch` tool executes, and inserted through
`Store::insert_snapshot`. Rows are keyed by the assistant message and that
message's dense `seq`, and `path` is the resolved absolute path. Only paths
inside the session directory are captured; anything escaping lexically or via a
symlink is skipped. A path that does not exist yet is recorded with
`existed=0` so revert deletes it. Per-file content is capped at 5 MiB;
unreadable, oversized or non-regular files are stored with `skipped=1` and no
content, and capture failure never fails the tool call. Boundaries are
inclusive (`seq >= boundary`) because the snapshot is taken before the
message's own write. Ids are `snap_…`.

## IDs

`Identifier` = 26 chars: 6 bytes of `(unix_ms * 0x1000 + counter24)` encoded
base62, followed by 14 random base62 chars. `ascending()` for events/messages,
`descending()` (bitwise-not) for sessions so newer sessions sort first.
Prefix by kind: `ses_`, `msg_`, `evt_`, `prt_` (provider call ids, not ours),
`per_`, `psv_`, `obs_`, `gol_`, `mem_`, `lan_`, `req_`, `frm_`, `cred_`,
`wkt_`, `sch_`, `snap_`. IDs are opaque on the wire; only `evt_`→`msg_`
replacement is contractual. `cred_`/`wkt_`/`frm_`/`sch_`/`snap_` are generated
by the store/runtime from the same ascending `EventId` body with the prefix
swapped.

## Write path: event-sourced, one transaction

For every durable change:

1. Acquire the per-session writer slot (in-process; `BEGIN IMMEDIATE` across
   processes).
2. Read/allocate `event_sequence.seq` for the session.
3. Run projectors **synchronously** in the same transaction:
   - insert/update `session_message`, `session_part` (diff ordinals; update only
     changed rows);
   - update `session_v2` counters/timestamps;
   - update `instruction_entry`/`instruction_state`;
   - insert `event` row (we persist durable events by default; upstream leaves
     this off, but we need `/log` replay and resync).
4. Commit.
5. Only then publish to the bus and wake waiters.

Consequences:

- `seq` is dense per session across durable event types; gaps are normal
  because ephemeral events never reserve a seq.
- A crash cannot produce a message whose seq is not committed.
- Ephemeral deltas never touch disk. Text/reasoning/tool-input fragments are
  persisted once at `*.ended` (or step end), not per delta.

Batching: one transaction per durable event, not per delta. A step emits
~8–20 durable events. Commit p95 ≤ 2 ms; no fsync per event (WAL + NORMAL).
If a transaction fails, the step fails with `StorageError` and the observer is
notified; partial projections are impossible.

## Read path

- **One server process owns a database.** `Store::open` takes an exclusive
  sidecar lock (`<db>.lock`) for the process lifetime. A second process opening
  the same file fails with a conflict instead of racing the live process's
  actors and boot recovery. The OS releases the lock on exit or hard kill
  (Windows sharing mode; Linux pid check).
- **Message page**: keyset on `seq`; cursor = base64url JSON
  `{id, order, direction}`; anchor resolved to the message's `seq`; `previous`
  queries reversed and re-reverses the page. Default limit 50, max 200.
- **Hydration**: parts loaded in batches of 500 by `(message_id, ordinal)`;
  legacy inline `content` is used when no part rows exist. This duality is
  required for imported DBs.
- **Context** (model view): find the latest completed compaction
  (`json_extract(data,'$.status')='completed'`), read rows from its seq, apply
  the top-level `keepID`, stop at the first unsettled assistant for `preview`.
- **Session list**: keyset on `(time_updated,id)`; filters directory/project/
  parentID/archived; search is `LIKE` on title (documented table scan, capped
  at 200 rows) — a future FTS index is allowed but must not change results.
- **Context reload (current behavior)**: the runtime reloads the full context
  from the store at the start of an execution and after each completed step,
  plus after a steer promotion or compaction. The `through_seq` column and
  keyset read path exist, but the runtime does not yet keep a decoded tail;
  an incremental watermark is a documented optimization, not landed behavior.

## Revert, fork, delete

- **Revert stage**: verify the boundary message exists (404 otherwise), read
  `earliest_snapshots_after(session_id, boundary_seq)`, diff each captured
  pre-image against the current file (`added`/`deleted`/`modified`, full-file
  hunks), store `session_v2.revert = {messageID, files}`, and return it.
- **Revert clear**: restore the earliest snapshot of every path at/after the
  boundary (rewrite the pre-image, or delete a file that did not exist), drop
  those snapshot rows, then null `session_v2.revert`. Paths outside the session
  directory, skipped captures and non-regular files are left untouched.
- **Revert commit**: resolve the boundary message's `seq`, call
  `truncate_session(seq)` (every `session_message`/`session_inbox` at or after
  it), call `delete_snapshots_after(seq)`, then null `session_v2.revert`.
- **Fork**: create a new session row (fresh `ses_` id, `parent_id = NULL`,
  same project/directory/title/agent/model/metadata), copy messages below the
  boundary with **new** message ids and fresh `seq` values (compaction
  top-level `keepID` is remapped to the copied message), then publish a durable
  `session.forked` v2 event on the child with
  `{sessionID, parentID, boundary}` where `boundary` is
  `{type:"before",messageID}` or `{type:"through",messageID}`. A fork with no
  history publishes no boundary event.
- **Delete**: check the session exists, publish durable `session.deleted`, then
  `delete_session`, which walks the recursive child tree, deletes every
  descendant row, decrements the parent's `child_count`, and drops each
  deleted aggregate's `event_sequence` row (its `event` rows cascade). Inbox
  rows, instruction state, forms and blob refs go through `session_v2` FK
  cascades.

## Crash recovery

On boot, per session with a claim:

- Assistant messages in `running` with no terminal event → `error`
  (`STEP_INTERRUPTED`), running tools → `cancelled`, running compaction →
  `failed`.
- Deliverable inbox rows → wake the actor; stale `delivering` rows → reset to
  `queued`.
- Background child jobs are not resumed automatically: the completion watcher was
  process-local and died with the old process. A repaired child is settled to
  its parent with exactly one failed `<subagent state="failed">` envelope
  (deduped against the parent transcript and pending inbox), so the parent can
  decide whether to restart it. Foreground parents also see their interrupted
  tool part repaired to `ToolAbortedError`.
- Orphan `blob` rows older than 24 h with no refs → delete (maintenance task,
  not boot-critical).
- Suspended sessions resume with concurrency ≤ 8, queued FIFO.

## Retention

Design target, **not implemented yet**: the store exposes no sweep/list API and
no maintenance task runs on boot or on a timer. Until that task exists, rows
accumulate; nothing is silently deleted.

| Data | Planned policy |
|---|---|
| `session_message`/`session_part` | keep; user can delete a session |
| `event` | 30 days or 100k rows per session, whichever first (replay is best-effort) |
| `observation` | 24 h or 10k rows per root |
| `observer_repair` | 30 days |
| `memory` | soft delete → hard delete after 30 days; cap 50k |
| `lane_ticket` | 7 days |
| `tool-output/` files | 7 days (hourly sweep) |
| `blob` orphans | 24 h |

When implemented it should be a single task every 10 min off the hot path,
bounded to 50 ms of work per tick (WAL-friendly).

## Import from upstream

Design target, **not implemented yet**: `oc-cli` only exposes `serve`, there is
no `db import` subcommand and no `OC_IMPORT_UPSTREAM` first-boot path.
`GET /api/experimental/migration/v1` answers `501 NotImplementedError` rather
than inventing migration state. The intended shape, for whoever builds it:

1. Open source read-only; verify `session_v2` exists.
2. Copy wire-compatible tables in FK order, preserving IDs and `seq`.
3. Copy `event_sequence` and drop `event` rows (we regenerate on demand).
4. Set `event_sequence.seq` to `max(existing seq) + 1` per session.
5. Never copy credentials without `--with-credentials`.
6. Verify counts and max seq; print a report; refuse partial import.

V1 databases are rejected with a clear message (phase 5 decision).
