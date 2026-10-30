//! File snapshots: the pre-write bytes of session files, captured before a
//! mutating tool runs.
//!
//! A snapshot row is keyed to the message whose tool call is about to change a
//! path (`message_id`, `tool_call_id`) and carries that message's dense `seq`,
//! so a revert boundary can address every snapshot taken at or after it. The
//! runtime captures the rows; the session extension consumes them:
//!
//! - [`Store::snapshots_after`] lists the captured history at or after a
//!   boundary, in capture order.
//! - [`Store::earliest_snapshots_after`] reduces that history to the first
//!   capture of every path, which is the file state a revert restores and the
//!   "before" side of a diff.
//! - [`Store::delete_snapshots_after`] drops the history once a revert commits,
//!   matching [`Store::truncate_session`]'s `seq >= boundary` deletion.
//!
//! All boundaries are inclusive: the snapshot a message takes is captured
//! before that message's own writes, so a revert *through* the message must
//! still see it.

use rusqlite::{Row, params};

use crate::db::{Store, in_tx};
use crate::error::StoreError;
use crate::session::ensure_session;

const SNAPSHOT_COLUMNS: &str =
    "id, session_id, message_id, seq, tool_call_id, path, existed, skipped, content, time_created";

/// Input for [`Store::insert_snapshot`].
#[derive(Debug, Clone)]
pub struct NewSnapshot {
    /// Row id.
    pub id: String,
    /// Owning session; must already exist.
    pub session_id: String,
    /// Message whose tool call is about to write.
    pub message_id: String,
    /// Dense per-session sequence number of [`Self::message_id`].
    pub seq: i64,
    /// Tool call that triggered the capture.
    pub tool_call_id: String,
    /// Absolute path that was snapshotted.
    pub path: String,
    /// Whether the path existed before the write.
    pub existed: bool,
    /// The path could not be captured (unreadable or above the size cap).
    pub skipped: bool,
    /// Pre-write bytes; `None` when the file was new or the capture was skipped.
    pub content: Option<Vec<u8>>,
    /// Capture time in epoch milliseconds.
    pub time_created: i64,
}

/// One stored snapshot row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRow {
    /// Row id.
    pub id: String,
    /// Owning session.
    pub session_id: String,
    /// Message whose tool call was about to write.
    pub message_id: String,
    /// Dense per-session sequence number of [`Self::message_id`].
    pub seq: i64,
    /// Tool call that triggered the capture.
    pub tool_call_id: String,
    /// Absolute path that was snapshotted.
    pub path: String,
    /// Whether the path existed before the write.
    pub existed: bool,
    /// The path could not be captured (unreadable or above the size cap).
    pub skipped: bool,
    /// Pre-write bytes; `None` when the file was new or the capture was skipped.
    pub content: Option<Vec<u8>>,
    /// Capture time in epoch milliseconds.
    pub time_created: i64,
}

impl SnapshotRow {
    fn read(row: &Row<'_>) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.get(0)?,
            session_id: row.get(1)?,
            message_id: row.get(2)?,
            seq: row.get(3)?,
            tool_call_id: row.get(4)?,
            path: row.get(5)?,
            existed: row.get(6)?,
            skipped: row.get(7)?,
            content: row.get(8)?,
            time_created: row.get(9)?,
        })
    }
}

impl Store {
    /// Stores one captured snapshot.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when the session does not exist; a duplicate
    /// row id is reported as a SQLite constraint failure.
    pub async fn insert_snapshot(&self, new: NewSnapshot) -> Result<(), StoreError> {
        let NewSnapshot {
            id,
            session_id,
            message_id,
            seq,
            tool_call_id,
            path,
            existed,
            skipped,
            content,
            time_created,
        } = new;
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                transaction.execute(
                    "INSERT INTO snapshot
                        (id, session_id, message_id, seq, tool_call_id, path,
                         existed, skipped, content, time_created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        &id,
                        &session_id,
                        &message_id,
                        seq,
                        &tool_call_id,
                        &path,
                        existed,
                        skipped,
                        content,
                        time_created
                    ],
                )?;
                Ok(())
            })
        })
        .await
    }

    /// Lists every snapshot of a session taken at or after `seq`, ordered by
    /// `seq` then `id`.
    pub async fn snapshots_after(
        &self,
        session_id: &str,
        seq: i64,
    ) -> Result<Vec<SnapshotRow>, StoreError> {
        let session_id = session_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM snapshot
                 WHERE session_id = ?1 AND seq >= ?2
                 ORDER BY seq ASC, id ASC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params![&session_id, seq])?;
            let mut snapshots = Vec::new();
            while let Some(row) = rows.next()? {
                snapshots.push(SnapshotRow::read(row)?);
            }
            Ok(snapshots)
        })
        .await
    }

    /// Returns the earliest snapshot of every path at or after `seq`: one row
    /// per path with the minimum `seq` (minimum `id` on a tie), ordered by
    /// `seq` then `id`.
    pub async fn earliest_snapshots_after(
        &self,
        session_id: &str,
        seq: i64,
    ) -> Result<Vec<SnapshotRow>, StoreError> {
        let session_id = session_id.to_owned();
        self.read(move |connection| {
            let sql = format!(
                "SELECT {SNAPSHOT_COLUMNS} FROM snapshot AS candidate
                 WHERE candidate.session_id = ?1 AND candidate.seq >= ?2
                   AND NOT EXISTS (
                     SELECT 1 FROM snapshot AS earlier
                     WHERE earlier.session_id = candidate.session_id
                       AND earlier.path = candidate.path
                       AND earlier.seq >= ?2
                       AND (earlier.seq < candidate.seq
                            OR (earlier.seq = candidate.seq AND earlier.id < candidate.id))
                   )
                 ORDER BY candidate.seq ASC, candidate.id ASC"
            );
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params![&session_id, seq])?;
            let mut snapshots = Vec::new();
            while let Some(row) = rows.next()? {
                snapshots.push(SnapshotRow::read(row)?);
            }
            Ok(snapshots)
        })
        .await
    }

    /// Deletes every snapshot of a session taken at or after `seq`, reporting
    /// how many rows were removed.
    ///
    /// The boundary matches [`Store::truncate_session`]: a revert commit passes
    /// the boundary message's `seq` and clears both the message tail and the
    /// snapshots captured for it.
    pub async fn delete_snapshots_after(
        &self,
        session_id: &str,
        seq: i64,
    ) -> Result<u64, StoreError> {
        let session_id = session_id.to_owned();
        self.write(move |connection| {
            let removed = connection.execute(
                "DELETE FROM snapshot WHERE session_id = ?1 AND seq >= ?2",
                params![&session_id, seq],
            )?;
            Ok(u64::try_from(removed).unwrap_or(u64::MAX))
        })
        .await
    }
}
