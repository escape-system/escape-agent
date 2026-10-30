# 06 — Permanent memory

Memory is durable, project-scoped, and injected as a cache-stable instruction
source. It is a SQLite feature, not a vector database: no embedding model runs
by default, no extra process, no startup cost.

## Data model

```sql
CREATE TABLE memory (
  id            TEXT PRIMARY KEY,          -- mem_…
  scope         TEXT NOT NULL,             -- global | project | agent | session
  scope_id      TEXT,                      -- project id / agent id / session id
  kind          TEXT NOT NULL,             -- fact | decision | preference | pattern | gotcha
  text          TEXT NOT NULL,
  tags          TEXT NOT NULL DEFAULT '[]',
  importance    INTEGER NOT NULL DEFAULT 3, -- 1..5
  confidence    REAL NOT NULL DEFAULT 0.7,
  source        TEXT NOT NULL,             -- user | agent | scribe | import
  source_session TEXT,
  source_message TEXT,
  hash          TEXT NOT NULL,             -- sha256 of normalized text
  created       INTEGER NOT NULL,
  updated       INTEGER NOT NULL,
  last_used     INTEGER,
  uses          INTEGER NOT NULL DEFAULT 0,
  deleted       INTEGER NOT NULL DEFAULT 0,
  UNIQUE(scope, scope_id, hash)
);

CREATE VIRTUAL TABLE memory_fts USING fts5(
  text, tags, content='memory', content_rowid='rowid', tokenize='unicode61'
);
```

`memory` also gets ordinary indices on `(scope, scope_id, deleted)`,
`(importance DESC, updated DESC)`, and `(last_used)`.

## Scope resolution

| Scope | Where | Visible to |
|---|---|---|
| `global` | `~/.config/opencode` | every session |
| `project` | project id | sessions in that project |
| `agent` | agent id | sessions running that agent |
| `session` | session id | that session and its descendants |

Retrieval merges `global + project + current agent`, plus `session` when the
caller is inside that subtree. Later scopes lose ties to earlier ones only on
identical text.

## Retrieval

Hybrid score, computed in one SQL statement:

```
score = bm25(memory_fts) * 1.0
      + importance * 0.35
      + recency_decay(last_used ?? updated) * 0.25
      + tag_overlap * 0.20
```

- Query = last user message (trimmed to 400 chars) + active goal objective +
  current skill ids.
- `top_k = 8`, token cap 800, hard cap 12.
- Pinned memories (`importance = 5`) are always included, even if the query
  does not match, up to 3.
- Results are deduped by `hash` and by near-duplicate (token Jaccard ≥ 0.85).
- Every injected memory bumps `uses` and `last_used` in one batched write at
  turn end, not per read.

Rendered block (instruction source `oc/memory`, hash-stable per result set):

```
<memory scope="project">
- [decision] Use rusqlite with a single writer; sqlx adds compile time we don't need. (mem_…)
- [gotcha] cargo test locks the target dir; use lanes. (mem_…)
</memory>
```

The block is part of the cached instruction prefix. When the result set
changes, it is emitted as an `instructions.updated` delta, exactly like
AGENTS.md changes.

## Tools

```jsonc
memory_write  { text, kind?, scope?, tags?, importance?, confidence?, source? }
memory_search { query, scope?, kinds?, tags?, limit? }
memory_forget { id }                       // soft delete
memory_list   { scope?, kind?, limit?, cursor? }
```

- `memory_write` from an agent requires permission `memory:write` (default
  `ask`); writes from the scribe do not.
- Text is capped at 500 chars. Longer "notes" belong in files, not memory.
- Writes are idempotent by `(scope, scope_id, hash)`; a repeat updates
  importance/confidence instead of duplicating.
- `memory_search` returns id, kind, text, score; no provenance dump.

## Auto-scribe

After a root turn settles (or every 6 steps), an asynchronous task runs the
`digest_model` with:

```
recent user intent + assistant final text + tool outcomes (one line each)
→ [{"text","kind","tags","importance","confidence"}]  (max 5)
```

Filters: confidence ≥ 0.6, text 20–500 chars, not a duplicate, not a secret
(regex for keys/tokens), not transient ("currently running…"). Scribe writes
are marked `source:"scribe"` and are visible/deletable in the UI. The scribe
never blocks the turn and is skipped when the session has no goal and no tools
ran.

## Retention

- Soft delete, then hard delete after 30 days.
- Cap: 50,000 rows per database; prune lowest `score = importance * uses *
  recency` first, never pinned.
- `VACUUM` only on explicit maintenance, not on boot.
- FTS rebuild on schema upgrade only.

## Optional embeddings

`memory.embedding.model` enables semantic recall:

- Embeddings stored as `BLOB` float32 (normalized), dimension from the model.
- Brute-force cosine top-32 over candidates pre-filtered by scope and recency;
  merge with FTS scores. Acceptable up to 50k rows.
- If the provider is unavailable, retrieval silently falls back to FTS and
  marks `semantic_degraded=true` in the block (not an error).
- Default off: 0 MB RAM, 0 startup latency.

## Memory vs. context vs. files

| Need | Use |
|---|---|
| Durable fact across sessions | `memory_write` |
| Large document / spec | file + `read` |
| Task state within a turn | goal + context |
| What a subagent did | `digest` (not memory) |

The agent is instructed (in the default system prompt) not to write memory for
things derivable from the repo, and to write at most one memory per turn unless
the user asks.

## Security

- Memory is local-only; never uploaded, never shared.
- Secrets are rejected by a detector (API-key shapes, JWTs, private keys) and
  the write fails with a reason.
- `memory_list`/search respect scope: a subagent cannot read another project's
  memory.
- Deletion is immediate for reads (soft-deleted rows are filtered), hard after
  retention.
