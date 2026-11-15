//! Per-model usage rollups over the assistant message log.
//!
//! `session_message.data` already carries `model`, `cost` and `tokens`, so the
//! rollup is a `json_extract` GROUP BY and needs no extra table. The message
//! discriminator lives in the `type` column, not in `data`.

use rusqlite::Connection;

use crate::db::Store;
use crate::error::StoreError;

/// Hard cap on grouped `(provider, model)` rows returned by [`model_usage`],
/// so one pathological database cannot produce an unbounded response.
pub const MAX_USAGE_ROWS: usize = 200;

/// One `(provider, model)` usage rollup.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsageRow {
    /// Provider id from `data.model.providerID`.
    pub provider_id: String,
    /// Model id from `data.model.id`.
    pub model_id: String,
    /// Number of assistant messages.
    pub requests: i64,
    /// Summed `data.tokens.input`.
    pub tokens_in: i64,
    /// Summed `data.tokens.output`.
    pub tokens_out: i64,
    /// Summed `data.tokens.reasoning`.
    pub tokens_reasoning: i64,
    /// Summed `data.cost` in USD.
    pub cost: f64,
}

const SQL: &str = "
SELECT json_extract(m.data,'$.model.providerID') provider_id,
       json_extract(m.data,'$.model.id') model_id,
       CAST(COUNT(*) AS INTEGER) requests,
       CAST(SUM(COALESCE(json_extract(m.data,'$.tokens.input'),0)) AS INTEGER) tokens_in,
       CAST(SUM(COALESCE(json_extract(m.data,'$.tokens.output'),0)) AS INTEGER) tokens_out,
       CAST(SUM(COALESCE(json_extract(m.data,'$.tokens.reasoning'),0)) AS INTEGER) tokens_reasoning,
       CAST(SUM(COALESCE(json_extract(m.data,'$.cost'),0.0)) AS REAL) cost
FROM session_message m
WHERE m.type='assistant' AND m.time_created >= ?1 AND json_extract(m.data,'$.model.id') IS NOT NULL
GROUP BY 1,2 ORDER BY cost DESC LIMIT ?2";

/// Roll up assistant messages created at or after `since_ms` (epoch ms).
///
/// `since_ms` is an absolute lower bound, not a window length: callers compute
/// `now - hours * 3_600_000` themselves. Rows without a model id are ignored,
/// missing cost and token fields count as zero, and the result is ordered by
/// cost descending with at most [`MAX_USAGE_ROWS`] grouped rows.
pub fn model_usage(
    connection: &Connection,
    since_ms: i64,
) -> Result<Vec<ModelUsageRow>, StoreError> {
    let mut statement = connection.prepare(SQL)?;
    let limit = i64::try_from(MAX_USAGE_ROWS).unwrap_or(i64::MAX);
    let mut rows = statement.query(rusqlite::params![since_ms, limit])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(ModelUsageRow {
            provider_id: row.get(0)?,
            model_id: row.get(1)?,
            requests: row.get(2)?,
            tokens_in: row.get(3)?,
            tokens_out: row.get(4)?,
            tokens_reasoning: row.get(5)?,
            cost: row.get(6)?,
        });
    }
    Ok(out)
}

impl Store {
    /// Per-model usage since `since_ms` (epoch ms), highest cost first.
    pub async fn model_usage(&self, since_ms: i64) -> Result<Vec<ModelUsageRow>, StoreError> {
        self.read(move |connection| model_usage(connection, since_ms))
            .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{NewMessage, NewSession};

    async fn seed(store: &Store) {
        store
            .create_session(NewSession {
                id: "ses_usage".to_owned(),
                parent_id: None,
                project_id: "prj_usage".to_owned(),
                directory: "/tmp/usage".to_owned(),
                title: None,
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                time_created: 1_000,
            })
            .await
            .expect("session");
    }

    async fn append(store: &Store, id: &str, kind: &str, data: serde_json::Value, at: i64) {
        store
            .append_message(NewMessage {
                id: id.to_owned(),
                session_id: "ses_usage".to_owned(),
                kind: kind.to_owned(),
                data,
                time_created: at,
            })
            .await
            .expect("message");
    }

    #[tokio::test]
    async fn rollup_sums_assistant_rows_and_ignores_other_types() {
        let store = Store::open_memory().await.expect("memory store");
        seed(&store).await;
        append(
            &store,
            "msg_a1",
            "assistant",
            json!({
                "model": {"providerID": "google", "id": "gemini-3-pro"},
                "cost": 0.5,
                "tokens": {"input": 100, "output": 20, "reasoning": 5}
            }),
            2_000,
        )
        .await;
        append(
            &store,
            "msg_a2",
            "assistant",
            json!({
                "model": {"providerID": "google", "id": "gemini-3-pro"},
                "cost": 0.25,
                "tokens": {"input": 50, "output": 10}
            }),
            3_000,
        )
        .await;
        // No cost and no tokens: one request with zero spend.
        append(
            &store,
            "msg_b1",
            "assistant",
            json!({"model": {"providerID": "opencode-go", "id": "glm-5.3-flash"}}),
            4_000,
        )
        .await;
        // A user row carrying a model must not count as usage.
        append(
            &store,
            "msg_user",
            "user",
            json!({
                "model": {"providerID": "google", "id": "gemini-3-pro"},
                "cost": 99.0
            }),
            5_000,
        )
        .await;
        // An assistant row without a model id is skipped by the WHERE clause.
        append(
            &store,
            "msg_no_model",
            "assistant",
            json!({"cost": 7.0}),
            6_000,
        )
        .await;

        let usage = store.model_usage(0).await.expect("rollup");
        assert_eq!(usage.len(), 2);
        let google = usage
            .iter()
            .find(|row| row.model_id == "gemini-3-pro")
            .expect("google row");
        assert_eq!(google.provider_id, "google");
        assert_eq!(google.requests, 2);
        assert_eq!(google.tokens_in, 150);
        assert_eq!(google.tokens_out, 30);
        assert_eq!(google.tokens_reasoning, 5);
        assert!((google.cost - 0.75).abs() < 1e-9, "cost {}", google.cost);

        let glm = usage
            .iter()
            .find(|row| row.model_id == "glm-5.3-flash")
            .expect("glm row");
        assert_eq!(glm.provider_id, "opencode-go");
        assert_eq!(glm.requests, 1);
        assert_eq!(glm.tokens_in, 0);
        assert_eq!(glm.tokens_out, 0);
        assert_eq!(glm.tokens_reasoning, 0);
        assert!(glm.cost.abs() < 1e-9, "cost {}", glm.cost);
    }

    #[tokio::test]
    async fn rollup_honours_the_since_bound() {
        let store = Store::open_memory().await.expect("memory store");
        seed(&store).await;
        append(
            &store,
            "msg_old",
            "assistant",
            json!({
                "model": {"providerID": "google", "id": "gemini-3-pro"},
                "cost": 1.0
            }),
            1_000,
        )
        .await;
        append(
            &store,
            "msg_new",
            "assistant",
            json!({
                "model": {"providerID": "opencode-go", "id": "glm-5.3-flash"},
                "cost": 2.0
            }),
            9_000,
        )
        .await;
        let recent = store.model_usage(5_000).await.expect("rollup");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].model_id, "glm-5.3-flash");
    }
}
