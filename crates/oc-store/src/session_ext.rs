//! Session maintenance that is not part of the event path: reverting by
//! sequence, resolving a message's sequence, and the "viewed" watermark.
//!
//! Wire messages do not expose `seq`, so callers that hold a message id (a
//! staged revert, for example) resolve it with [`Store::message_seq`] and pass
//! the result to [`Store::truncate_session`].

use rusqlite::{OptionalExtension, params};

use crate::db::{Store, in_tx};
use crate::error::StoreError;
use crate::session::ensure_session;

impl Store {
    /// The dense per-session `seq` of one message row, if it exists.
    pub async fn message_seq(&self, session_id: &str, id: &str) -> Result<Option<u64>, StoreError> {
        let session_id = session_id.to_owned();
        let id = id.to_owned();
        self.read(move |connection| {
            let seq: Option<i64> = connection
                .query_row(
                    "SELECT seq FROM session_message WHERE id = ?1 AND session_id = ?2",
                    params![&id, &session_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(seq.map(|seq| u64::try_from(seq).unwrap_or_default()))
        })
        .await
    }

    /// Deletes the session tail from `after_seq` onwards in one transaction.
    ///
    /// Every message with `seq >= after_seq` and every inbox item with
    /// `enqueued_seq >= after_seq` is removed; parts follow their message
    /// through the foreign-key cascade. The returned count is the number of
    /// message and inbox rows deleted (cascaded parts are not counted).
    ///
    /// The session row itself, its `event_sequence` reservation and any blobs
    /// are untouched; callers that revert afterwards rewind those separately.
    pub async fn truncate_session(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<u64, StoreError> {
        let session_id = session_id.to_owned();
        let boundary = i64::try_from(after_seq).unwrap_or(i64::MAX);
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                ensure_session(transaction, &session_id)?;
                let messages = transaction.execute(
                    "DELETE FROM session_message WHERE session_id = ?1 AND seq >= ?2",
                    params![&session_id, boundary],
                )?;
                let inbox = transaction.execute(
                    "DELETE FROM session_inbox WHERE session_id = ?1 AND enqueued_seq >= ?2",
                    params![&session_id, boundary],
                )?;
                Ok(u64::try_from(messages + inbox).unwrap_or(u64::MAX))
            })
        })
        .await
    }

    /// Records when the client last viewed the session.
    ///
    /// An unknown session is [`StoreError::NotFound`].
    pub async fn touch_session_viewed(&self, session_id: &str, at: i64) -> Result<(), StoreError> {
        let session_id = session_id.to_owned();
        self.write(move |connection| {
            let updated = connection.execute(
                "UPDATE session_v2 SET time_viewed = ?2 WHERE id = ?1",
                params![&session_id, at],
            )?;
            if updated == 0 {
                return Err(StoreError::NotFound(format!("session {session_id}")));
            }
            Ok(())
        })
        .await
    }
}
