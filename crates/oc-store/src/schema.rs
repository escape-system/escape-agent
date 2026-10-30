//! Embedded, ordered migrations.
//!
//! Every migration runs in its own transaction and is recorded in `migration`
//! with a checksum. A checksum mismatch refuses to start the store.

pub(crate) struct Migration {
    pub(crate) id: &'static str,
    pub(crate) sql: &'static str,
    /// When set, the migration is skipped (but still recorded) if the column
    /// already exists, so `ALTER TABLE ... ADD COLUMN` stays idempotent.
    pub(crate) column_guard: Option<(&'static str, &'static str)>,
}

/// Wire-compatible tables from `docs/03-storage.md`.
const WIRE: &str = r#"
CREATE TABLE project (
  id TEXT PRIMARY KEY,
  directory TEXT NOT NULL,
  canonical TEXT NOT NULL,
  name TEXT,
  icon TEXT,
  time_created INTEGER,
  time_updated INTEGER
);
CREATE INDEX project_directory_idx ON project(directory);

CREATE TABLE worktree (
  id TEXT PRIMARY KEY,
  project_id TEXT REFERENCES project(id) ON DELETE CASCADE,
  directory TEXT NOT NULL,
  canonical TEXT,
  branch TEXT,
  name TEXT,
  time_created INTEGER,
  time_updated INTEGER
);
CREATE INDEX worktree_project_idx ON worktree(project_id);

CREATE TABLE session_v2 (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  parent_id TEXT,
  root_id TEXT,
  depth INTEGER NOT NULL DEFAULT 0,
  role TEXT NOT NULL DEFAULT 'primary',
  spawn_allowed INTEGER NOT NULL DEFAULT 1,
  child_count INTEGER NOT NULL DEFAULT 0,
  fork_session_id TEXT,
  fork_boundary TEXT,
  directory TEXT,
  path TEXT,
  title TEXT,
  agent TEXT,
  metadata TEXT,
  permission TEXT,
  model TEXT,
  revert TEXT,
  summary_diffs TEXT,
  cost REAL NOT NULL DEFAULT 0,
  tokens_input INTEGER NOT NULL DEFAULT 0,
  tokens_output INTEGER NOT NULL DEFAULT 0,
  tokens_reasoning INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  time_busy INTEGER,
  time_idle INTEGER,
  time_viewed INTEGER,
  idle_outcome TEXT,
  time_compacting INTEGER,
  time_archived INTEGER,
  time_suspended INTEGER,
  resume_attempts INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX session_project_idx ON session_v2(project_id);
CREATE INDEX session_workspace_idx ON session_v2(directory);
CREATE INDEX session_parent_idx ON session_v2(parent_id);
CREATE INDEX session_time_idx ON session_v2(time_updated DESC, id DESC);
CREATE INDEX session_archived_idx ON session_v2(time_archived, time_updated DESC);

CREATE TABLE session_message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type TEXT NOT NULL,
  seq INTEGER NOT NULL,
  data TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  UNIQUE(session_id, seq)
);
CREATE INDEX message_session_type_seq ON session_message(session_id, type, seq);
CREATE INDEX message_session_time ON session_message(session_id, time_created, id);

CREATE TABLE session_part (
  message_id TEXT NOT NULL REFERENCES session_message(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type TEXT NOT NULL,
  call_id TEXT,
  data TEXT NOT NULL,
  PRIMARY KEY(message_id, ordinal)
);
CREATE INDEX part_session_message ON session_part(session_id, message_id, ordinal);

CREATE TABLE session_inbox (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  type TEXT NOT NULL,
  payload TEXT NOT NULL,
  delivery TEXT NOT NULL,
  enqueued_seq INTEGER NOT NULL,
  time_created INTEGER NOT NULL,
  UNIQUE(session_id, enqueued_seq)
);
CREATE INDEX inbox_delivery ON session_inbox(session_id, delivery, enqueued_seq);

CREATE TABLE event_sequence (
  aggregate_id TEXT PRIMARY KEY,
  seq INTEGER NOT NULL,
  owner_id TEXT
);

CREATE TABLE event (
  id TEXT PRIMARY KEY,
  aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  created INTEGER NOT NULL DEFAULT 0,
  type TEXT NOT NULL,
  data TEXT NOT NULL,
  UNIQUE(aggregate_id, seq)
);
CREATE INDEX event_aggregate_type_seq ON event(aggregate_id, type, seq);

CREATE TABLE blob (
  hash TEXT PRIMARY KEY,
  mime TEXT,
  bytes INTEGER,
  width INTEGER,
  height INTEGER,
  data BLOB NOT NULL,
  time_created INTEGER NOT NULL
);

CREATE TABLE blob_ref (
  session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE,
  message_id TEXT,
  hash TEXT REFERENCES blob(hash) ON DELETE CASCADE,
  seq INTEGER,
  PRIMARY KEY(session_id, message_id, hash)
);

CREATE TABLE instruction_entry (
  session_id TEXT REFERENCES session_v2(id) ON DELETE CASCADE,
  "key" TEXT,
  "value" TEXT,
  removed INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(session_id, "key")
);

CREATE TABLE instruction_state (
  session_id TEXT PRIMARY KEY REFERENCES session_v2(id) ON DELETE CASCADE,
  epoch_start INTEGER,
  through_seq INTEGER,
  initial_values TEXT,
  current_values TEXT
);

CREATE TABLE permission_request (
  id TEXT PRIMARY KEY,
  session_id TEXT,
  action TEXT,
  resources TEXT,
  save TEXT,
  metadata TEXT,
  source TEXT,
  message TEXT,
  time_created INTEGER NOT NULL
);
CREATE INDEX permission_request_session_idx ON permission_request(session_id, time_created);

CREATE TABLE permission_saved (
  id TEXT PRIMARY KEY,
  project_id TEXT,
  action TEXT,
  resource TEXT,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  UNIQUE(project_id, action, resource)
);

CREATE TABLE credential (
  id TEXT PRIMARY KEY,
  integration_id TEXT,
  label TEXT,
  "value" TEXT,
  active INTEGER
);

CREATE TABLE kv (
  "key" TEXT PRIMARY KEY,
  "value" TEXT NOT NULL
);
"#;

/// Additive tables for our own features (`docs/03-storage.md`).
const ADDITIVE: &str = r"
CREATE TABLE goal (
  id TEXT PRIMARY KEY,
  root_id TEXT NOT NULL,
  objective TEXT NOT NULL,
  criteria TEXT NOT NULL,
  status TEXT NOT NULL,
  rounds INTEGER DEFAULT 0,
  stalls INTEGER DEFAULT 0,
  last_progress INTEGER,
  evidence TEXT,
  model TEXT,
  time_created INTEGER,
  time_updated INTEGER
);
CREATE UNIQUE INDEX goal_active_root ON goal(root_id) WHERE status = 'active';

CREATE TABLE observation (
  id TEXT PRIMARY KEY,
  root_id TEXT,
  session_id TEXT,
  tool_call_id TEXT,
  tool TEXT,
  why TEXT,
  expect_s INTEGER,
  intervene_s INTEGER,
  started INTEGER,
  settled INTEGER,
  state TEXT,
  outcome TEXT,
  parent_call TEXT,
  synthetic INTEGER DEFAULT 0
);
CREATE INDEX observation_root_active ON observation(root_id, state, started);

CREATE TABLE observer_repair (
  id TEXT PRIMARY KEY,
  root_id TEXT,
  target_id TEXT,
  kind TEXT,
  reason TEXT,
  args_hash TEXT,
  attempt INTEGER,
  time_created INTEGER
);

CREATE TABLE memory (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL,
  scope_id TEXT,
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  tags TEXT NOT NULL DEFAULT '[]',
  importance INTEGER,
  confidence REAL,
  source TEXT,
  source_session TEXT,
  source_message TEXT,
  hash TEXT NOT NULL,
  created INTEGER,
  updated INTEGER,
  last_used INTEGER,
  uses INTEGER DEFAULT 0,
  deleted INTEGER DEFAULT 0,
  UNIQUE(scope, scope_id, hash)
);
CREATE VIRTUAL TABLE memory_fts USING fts5(text, tags, content='memory', content_rowid='rowid');

CREATE TABLE lane_ticket (
  id TEXT PRIMARY KEY,
  lane TEXT NOT NULL,
  command TEXT NOT NULL,
  cwd TEXT,
  priority INTEGER,
  owner_session TEXT,
  state TEXT,
  queued INTEGER,
  started INTEGER,
  finished INTEGER,
  exit_code INTEGER
);
";

/// Memory FTS sync (`docs/06-memory.md`). A separate migration so databases
/// created before memory support get the triggers and indices too.
const MEMORY_FTS: &str = r"
DROP TABLE IF EXISTS memory_fts;
CREATE VIRTUAL TABLE memory_fts USING fts5(
  text, tags, content='memory', content_rowid='rowid', tokenize='unicode61'
);
CREATE TRIGGER IF NOT EXISTS memory_fts_ai AFTER INSERT ON memory BEGIN
  INSERT INTO memory_fts(rowid, text, tags) VALUES (new.rowid, new.text, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memory_fts_ad AFTER DELETE ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, text, tags)
  VALUES ('delete', old.rowid, old.text, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS memory_fts_au AFTER UPDATE OF text, tags ON memory BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, text, tags)
  VALUES ('delete', old.rowid, old.text, old.tags);
  INSERT INTO memory_fts(rowid, text, tags) VALUES (new.rowid, new.text, new.tags);
END;
CREATE UNIQUE INDEX IF NOT EXISTS memory_scope_hash_idx
  ON memory(scope, coalesce(scope_id, ''), hash);
CREATE INDEX IF NOT EXISTS memory_scope_idx ON memory(scope, scope_id, deleted);
CREATE INDEX IF NOT EXISTS memory_importance_idx ON memory(importance DESC, updated DESC);
CREATE INDEX IF NOT EXISTS memory_last_used_idx ON memory(last_used);
INSERT INTO memory_fts(memory_fts) VALUES ('rebuild');
";

/// Project archival flag for the UI routes. Guarded so a fresh database whose
/// `project` table already carries the column still starts.
const PROJECT_ARCHIVED: &str = r"
ALTER TABLE project ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS project_archived_idx ON project(archived, time_updated DESC);
CREATE INDEX IF NOT EXISTS project_time_idx ON project(time_updated DESC, id DESC);
";

/// Durable forms (`docs/02-protocol.md`, `/api/session/:id/form`). Unlike
/// upstream (in-memory with a ten-minute TTL) forms survive a restart, so a
/// pending question can still be answered after a crash.
const FORM: &str = r"
CREATE TABLE form (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  title TEXT NOT NULL,
  metadata TEXT,
  fields TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending', 'replied', 'cancelled')),
  reply TEXT,
  cancel_message TEXT,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL
);
CREATE INDEX form_session_status_time
  ON form(session_id, status, time_created DESC, id DESC);
CREATE INDEX form_status_time ON form(status, time_created DESC, id DESC);
";

/// Lane ticket columns the wire `Ticket` needs beyond the original additive
/// table. Separate migrations so each `ALTER TABLE` has its own column guard.
const LANE_TICKET_MESSAGE: &str = "ALTER TABLE lane_ticket ADD COLUMN message TEXT;";
const LANE_TICKET_OUTPUT_PATH: &str = "ALTER TABLE lane_ticket ADD COLUMN output_path TEXT;";
const LANE_TICKET_QUEUED_IDX: &str = r"
CREATE INDEX IF NOT EXISTS lane_ticket_queued_idx
  ON lane_ticket(queued DESC, id DESC);
";

/// Durable schedules (`docs/02-protocol.md`, `/api/experimental/schedule`).
/// `when_json` and `action_json` hold the wire `Schedule.When` /
/// `Schedule.Action` objects verbatim; `tz` is the resolved zone the trigger is
/// evaluated in.
const SCHEDULE: &str = r"
CREATE TABLE schedule (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  name TEXT,
  when_json TEXT NOT NULL,
  tz TEXT NOT NULL,
  action_json TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active'
    CHECK (status IN ('active', 'paused', 'done')),
  next_due_at INTEGER,
  ends_at INTEGER,
  remaining_runs INTEGER,
  run_count INTEGER NOT NULL DEFAULT 0,
  last_fired_at INTEGER,
  last_outcome TEXT,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL
);
CREATE INDEX schedule_status_due_idx ON schedule(status, next_due_at);
CREATE INDEX schedule_session_idx ON schedule(session_id);
";

/// File snapshots captured before a write tool mutates a session file.
///
/// `seq` is the dense sequence number of the message the tool call belongs to,
/// so a revert boundary can find every snapshot taken at or after it. `existed`
/// records whether the path existed before the write; `skipped` marks a path
/// whose content could not be captured (unreadable or above the size cap).
/// `content` is the pre-write bytes, `NULL` when there was nothing to keep.
const SNAPSHOT: &str = r"
CREATE TABLE snapshot (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
  message_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  tool_call_id TEXT NOT NULL,
  path TEXT NOT NULL,
  existed INTEGER NOT NULL,
  skipped INTEGER NOT NULL,
  content BLOB,
  time_created INTEGER NOT NULL
);
CREATE INDEX snapshot_session_seq_path ON snapshot(session_id, seq, path);
";

pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        id: "0001_wire",
        sql: WIRE,
        column_guard: None,
    },
    Migration {
        id: "0002_additive",
        sql: ADDITIVE,
        column_guard: None,
    },
    Migration {
        id: "0003_memory_fts",
        sql: MEMORY_FTS,
        column_guard: None,
    },
    Migration {
        id: "0004_project_archived",
        sql: PROJECT_ARCHIVED,
        column_guard: Some(("project", "archived")),
    },
    Migration {
        id: "0005_form",
        sql: FORM,
        column_guard: None,
    },
    Migration {
        id: "0006_lane_ticket_message",
        sql: LANE_TICKET_MESSAGE,
        column_guard: Some(("lane_ticket", "message")),
    },
    Migration {
        id: "0007_lane_ticket_output_path",
        sql: LANE_TICKET_OUTPUT_PATH,
        column_guard: Some(("lane_ticket", "output_path")),
    },
    Migration {
        id: "0008_lane_ticket_queued_idx",
        sql: LANE_TICKET_QUEUED_IDX,
        column_guard: None,
    },
    Migration {
        id: "0009_schedule",
        sql: SCHEDULE,
        column_guard: None,
    },
    Migration {
        id: "0010_snapshot",
        sql: SNAPSHOT,
        column_guard: None,
    },
];
