//! Session update paths: header patches, busy/idle transitions with their
//! durable reason rule, and step usage accumulation.

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter};

use crate::db::{Store, in_tx, now_ms};
use crate::error::StoreError;

use super::SessionPatch;
use super::value::json_sql;

impl Store {
    /// Applies a patch to a session. Unknown session ids are a `NotFound` error.
    pub async fn patch_session(&self, id: &str, patch: SessionPatch) -> Result<(), StoreError> {
        let id = id.to_owned();
        let updated = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let mut assignments: Vec<&'static str> = Vec::new();
                let mut values: Vec<SqlValue> = Vec::new();
                if let Some(title) = &patch.title {
                    assignments.push("title = ?");
                    values.push(SqlValue::Text(title.clone()));
                }
                if let Some(agent) = &patch.agent {
                    assignments.push("agent = ?");
                    values.push(SqlValue::Text(agent.clone()));
                }
                if let Some(model) = &patch.model {
                    assignments.push("model = ?");
                    values.push(json_sql(Some(model))?);
                }
                if let Some(metadata) = &patch.metadata {
                    assignments.push("metadata = ?");
                    values.push(json_sql(Some(metadata))?);
                }
                if let Some(permissions) = &patch.permissions {
                    assignments.push("permission = ?");
                    values.push(json_sql(Some(permissions))?);
                }
                if let Some(directory) = &patch.directory {
                    assignments.push("directory = ?");
                    values.push(SqlValue::Text(directory.clone()));
                }
                if let Some(path) = &patch.path {
                    assignments.push("path = ?");
                    values.push(SqlValue::Text(path.clone()));
                }
                if let Some(revert) = &patch.revert {
                    assignments.push("revert = ?");
                    values.push(json_sql(Some(revert))?);
                }
                if let Some(archived) = patch.archived {
                    assignments.push("time_archived = ?");
                    values.push(if archived {
                        SqlValue::Integer(updated)
                    } else {
                        SqlValue::Null
                    });
                }
                assignments.push("time_updated = ?");
                values.push(SqlValue::Integer(updated));
                values.push(SqlValue::Text(id.clone()));

                let sql = format!(
                    "UPDATE session_v2 SET {} WHERE id = ?",
                    assignments.join(", ")
                );
                if transaction.execute(&sql, params_from_iter(values.iter()))? == 0 {
                    return Err(StoreError::NotFound(format!("session {id}")));
                }
                Ok(())
            })
        })
        .await
    }

    /// Records the start of an active execution on `session_v2`.
    ///
    /// Unknown session ids are a `NotFound` error: the caller publishes
    /// `status:busy` and `execution.started` immediately after this write, so a
    /// silent no-op would announce a durable busy state that does not exist.
    pub async fn mark_session_busy(&self, id: &str, time_busy: i64) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.write(move |connection| {
            let changed = connection.execute(
                "UPDATE session_v2
                 SET time_busy = max(?1, coalesce(time_idle + 1, ?1)),
                     idle_outcome = NULL,
                     idle_reason = NULL,
                     time_updated = max(time_updated, ?1)
                 WHERE id = ?2",
                params![time_busy, id],
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound(format!("session {id}")));
            }
            Ok(())
        })
        .await
    }

    /// Records the completion of an execution on `session_v2`.
    pub async fn mark_session_idle(
        &self,
        id: &str,
        time_idle: i64,
        outcome: &str,
    ) -> Result<(), StoreError> {
        self.mark_session_idle_with_reason(id, time_idle, outcome, None)
            .await
    }

    /// Records the completion of an execution and, when the reason is
    /// meaningful, why it ended (`user`, `step limit reached after N steps`).
    ///
    /// `idle_reason` is durable for three shapes only: an `interrupted` outcome
    /// (why the turn was stopped), a `step_limit` outcome (the internal literal a
    /// step-limited turn stores; the reason is the step-limit grammar), and a
    /// `succeeded` outcome whose reason is that same grammar
    /// ([`oc_types::parse_step_limit`]) — the turn answered, but its step budget,
    /// not the model, ended it. Any other pair stores `NULL`, so a later voluntary
    /// turn always clears a previous reason.
    pub async fn mark_session_idle_with_reason(
        &self,
        id: &str,
        time_idle: i64,
        outcome: &str,
        reason: Option<&str>,
    ) -> Result<(), StoreError> {
        let id = id.to_owned();
        let outcome = outcome.to_owned();
        let reason = reason
            .filter(|reason| keeps_idle_reason(Some(outcome.as_str()), reason))
            .map(str::to_owned);
        self.write(move |connection| {
            let changed = connection.execute(
                "UPDATE session_v2
                 SET time_idle = max(?1, coalesce(time_busy, ?1)),
                     idle_outcome = ?2,
                     idle_reason = ?3,
                     time_updated = max(time_updated, ?1)
                 WHERE id = ?4",
                params![time_idle, outcome, reason, id],
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound(format!("session {id}")));
            }
            Ok(())
        })
        .await
    }

    /// Accumulates step token and cost usage onto `session_v2`.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_session_usage(
        &self,
        id: &str,
        cost: f64,
        tokens_input: i64,
        tokens_output: i64,
        tokens_reasoning: i64,
        tokens_cache_read: i64,
        tokens_cache_write: i64,
    ) -> Result<(), StoreError> {
        let id = id.to_owned();
        let updated = now_ms();
        self.write(move |connection| {
            let changed = connection.execute(
                "UPDATE session_v2
                 SET cost = cost + ?1,
                     tokens_input = tokens_input + ?2,
                     tokens_output = tokens_output + ?3,
                     tokens_reasoning = tokens_reasoning + ?4,
                     tokens_cache_read = tokens_cache_read + ?5,
                     tokens_cache_write = tokens_cache_write + ?6,
                     time_updated = max(time_updated, ?7)
                 WHERE id = ?8",
                params![
                    cost,
                    tokens_input,
                    tokens_output,
                    tokens_reasoning,
                    tokens_cache_read,
                    tokens_cache_write,
                    updated,
                    id,
                ],
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound(format!("session {id}")));
            }
            Ok(())
        })
        .await
    }
}

/// Whether a durable `idle_reason` survives next to `outcome`.
///
/// Only these pairs are durable: `interrupted` + any reason (why it stopped),
/// `step_limit` + any reason (the internal step-limited outcome), and
/// `succeeded` + the strict step-limit grammar (rows written before the
/// `step_limit` literal existed). Everything else — including a `succeeded`
/// row with an unrelated reason — is stored as `NULL`, so a voluntary turn
/// clears any previous reason and the interrupted-only rule stays intact for
/// every other outcome.
pub(crate) fn keeps_idle_reason(outcome: Option<&str>, reason: &str) -> bool {
    match outcome {
        Some("interrupted" | "step_limit") => true,
        Some("succeeded") => oc_types::parse_step_limit(reason).is_some(),
        _ => false,
    }
}
