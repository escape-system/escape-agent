//! The keyset list query over `session_v2`.
//!
//! The walk is frozen at its first page: that page reads
//! `snapshot = max(time_updated)` over the filtered rows in the same statement as
//! the page, and every later page bounds itself by the value carried in the
//! cursor. See [`super::pagination`] for the exact contract, including the residual
//! hazard that a row touched to a time still inside the snapshot can move across
//! the cursor.

use std::fmt::Write as _;

use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde_json::Value;

use crate::cursor::{Direction, Order};
use crate::db::Store;
use crate::error::StoreError;

use super::pagination;
use super::value::session_value;
use super::{SESSION_COLUMNS, SessionPage, SessionQuery};

/// Appends `value` and returns the numbered placeholder that binds it.
///
/// Numbered parameters are what let one filter fragment be referenced twice (in
/// the snapshot CTE and in the outer `WHERE`) while its value is pushed once. The
/// snapshot CTE appears textually before the filters, so the numbering — not the
/// textual order — has to determine the binding.
fn placeholder(values: &mut Vec<SqlValue>, value: SqlValue) -> String {
    values.push(value);
    format!("?{}", values.len())
}

/// Column alias of the walk's frozen `max(time_updated)`; appended after
/// [`SESSION_COLUMNS`] so every positional index in `session_value` stays put.
const SNAPSHOT_ALIAS: &str = "snapshot_bound";

impl Store {
    /// Lists sessions with keyset pagination.
    #[allow(clippy::too_many_lines)] // one linear keyset query; splitting hides the cursor contract
    pub async fn list_sessions(&self, query: SessionQuery) -> Result<SessionPage, StoreError> {
        let mut query = query;
        let anchor = pagination::resolve(&mut query)?;
        let effective = anchor.sql_order();

        let mut conditions: Vec<String> = Vec::new();
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(search) = &anchor.filters.search {
            let bound = placeholder(&mut values, SqlValue::Text(format!("%{search}%")));
            conditions.push(format!("title LIKE {bound}"));
        }
        match &anchor.filters.parent {
            Some(Some(parent)) => {
                let bound = placeholder(&mut values, SqlValue::Text(parent.clone()));
                conditions.push(format!("parent_id = {bound}"));
            }
            Some(None) => conditions.push("parent_id IS NULL".to_owned()),
            None => {}
        }
        if let Some(archived) = anchor.filters.archived {
            let condition = if archived {
                "time_archived IS NOT NULL"
            } else {
                "time_archived IS NULL"
            };
            conditions.push(condition.to_owned());
        }
        if let Some(directory) = &anchor.filters.directory {
            let bound = placeholder(&mut values, SqlValue::Text(directory.clone()));
            conditions.push(format!("directory = {bound}"));
        }
        if let Some(project) = &anchor.filters.project {
            let bound = placeholder(&mut values, SqlValue::Text(project.clone()));
            conditions.push(format!("project_id = {bound}"));
        }
        if let Some(subpath) = &anchor.filters.subpath {
            let bound = placeholder(&mut values, SqlValue::Text(subpath.clone()));
            conditions.push(format!("path = {bound}"));
        }
        if let Some((time, id)) = &anchor.position {
            let comparator = if effective == Order::Asc { ">" } else { "<" };
            let at = placeholder(&mut values, SqlValue::Integer(*time));
            let after = placeholder(&mut values, SqlValue::Text(id.clone()));
            conditions.push(format!(
                "(time_updated {comparator} {at} OR (time_updated = {at} AND id {comparator} {after}))"
            ));
        }

        let filter_sql = conditions.join(" AND ");
        // The first page always bounds the walk; a cursor bounds it only when it
        // carries a snapshot. A cursor without one predates the field and keeps
        // the pre-freeze, unbounded behaviour for the rest of its walk.
        let bounded = anchor.position.is_none() || anchor.snapshot.is_some();

        let mut sql = String::new();
        if let Some(snapshot) = anchor.snapshot {
            let bound = placeholder(&mut values, SqlValue::Integer(snapshot));
            write!(sql, "WITH snapshot(v) AS (SELECT {bound}) ")
                .expect("writing to a String cannot fail");
        } else if anchor.position.is_none() {
            // Same statement as the page, so the bound and the rows come from one
            // implicit read transaction: a second round trip could observe a
            // different database state and freeze a bound the page never saw.
            let scope = if filter_sql.is_empty() {
                String::new()
            } else {
                format!(" WHERE {filter_sql}")
            };
            write!(
                sql,
                "WITH snapshot(v) AS (SELECT max(time_updated) FROM session_v2{scope}) "
            )
            .expect("writing to a String cannot fail");
        }
        write!(sql, "SELECT {SESSION_COLUMNS}").expect("writing to a String cannot fail");
        if bounded {
            write!(sql, ", (SELECT v FROM snapshot) AS {SNAPSHOT_ALIAS}")
                .expect("writing to a String cannot fail");
        } else {
            write!(sql, ", NULL AS {SNAPSHOT_ALIAS}").expect("writing to a String cannot fail");
        }
        sql.push_str(" FROM session_v2");
        let mut where_sql = filter_sql;
        if bounded {
            if !where_sql.is_empty() {
                where_sql.push_str(" AND ");
            }
            where_sql.push_str("time_updated <= (SELECT v FROM snapshot)");
        }
        if !where_sql.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&where_sql);
        }
        let limit = placeholder(
            &mut values,
            SqlValue::Integer(i64::try_from(query.limit + 1).unwrap_or(i64::MAX)),
        );
        write!(
            sql,
            " ORDER BY time_updated {}, id {} LIMIT {limit}",
            effective.sql(),
            effective.sql()
        )
        .expect("writing to a String cannot fail");

        let (rows, frozen) = self
            .read(move |connection| {
                let mut statement = connection.prepare(&sql)?;
                let mut mapped = statement.query(params_from_iter(values.iter()))?;
                let mut items: Vec<(i64, String, Value)> = Vec::new();
                let mut frozen: Option<i64> = None;
                while let Some(row) = mapped.next()? {
                    let id: String = row.get(0)?;
                    let time: i64 = row.get(26)?;
                    let bound: Option<i64> = row.get(SNAPSHOT_ALIAS)?;
                    frozen = bound.or(frozen);
                    items.push((time, id, session_value(row)?));
                }
                Ok((items, frozen))
            })
            .await?;

        let has_more = rows.len() > query.limit;
        let mut rows = rows;
        rows.truncate(query.limit);
        if anchor.direction == Direction::Previous {
            rows.reverse();
        }

        let page_cursor = pagination::page_cursor(
            &rows,
            anchor.direction,
            anchor.order,
            anchor.position.as_ref(),
            frozen.or(anchor.snapshot),
            has_more,
            &anchor.filters,
        )?;

        Ok(SessionPage {
            items: rows.into_iter().map(|(_, _, value)| value).collect(),
            cursor: page_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    use super::*;
    use crate::{NewMessage, NewSession, SessionQuery};

    async fn seed(store: &Store, id: &str, at: i64) {
        store
            .create_session(NewSession {
                id: id.to_owned(),
                parent_id: None,
                project_id: "prj_1".to_owned(),
                directory: "/tmp/one".to_owned(),
                title: Some(id.to_owned()),
                agent: None,
                model: None,
                metadata: None,
                permissions: None,
                time_created: at,
            })
            .await
            .expect("create session");
    }

    /// Touches a session exactly like an appended message does (`touch_session`
    /// raises `time_updated` to at least the message time).
    async fn touch(store: &Store, session: &str, at: i64) {
        store
            .append_message(NewMessage {
                id: format!("msg_{session}_{at}"),
                session_id: session.to_owned(),
                kind: "user".to_owned(),
                data: json!({ "text": "bump" }),
                time_created: at,
            })
            .await
            .expect("append message");
    }

    fn ids(page: &SessionPage) -> Vec<String> {
        page.items
            .iter()
            .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn decode(raw: &str) -> Value {
        let bytes = URL_SAFE_NO_PAD.decode(raw).expect("cursor is base64url");
        serde_json::from_slice(&bytes).expect("cursor is json")
    }

    /// FAILS BEFORE THE FIX: unbounded, the ascending second page also returns the
    /// session created after page 1, so a walk reports a row that was not in the
    /// list when it started.
    #[tokio::test]
    async fn ascending_walk_excludes_sessions_created_after_the_first_page() {
        let store = Store::open_memory().await.expect("store");
        seed(&store, "ses_a", 1_000).await;
        seed(&store, "ses_b", 2_000).await;
        seed(&store, "ses_c", 3_000).await;

        let first = store
            .list_sessions(SessionQuery {
                limit: 2,
                order: Order::Asc,
                ..Default::default()
            })
            .await
            .expect("first page");
        assert_eq!(ids(&first), vec!["ses_a", "ses_b"]);
        let next = first.cursor.next.expect("next cursor");
        let payload = decode(&next);
        assert_eq!(
            payload["snapshot"].as_i64(),
            Some(3_000),
            "the first page freezes max(time_updated) in the cursor"
        );

        // A session created after the walk started.
        seed(&store, "ses_d", 4_000).await;

        let second = store
            .list_sessions(SessionQuery {
                cursor: Some(next),
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("second page");
        assert_eq!(
            ids(&second),
            vec!["ses_c"],
            "a walk is the list as of its first page"
        );
    }

    /// FAILS BEFORE THE FIX: unbounded, an already-visited session that is touched
    /// past the cursor comes back a second time later in the same ascending walk.
    #[tokio::test]
    async fn ascending_walk_does_not_repeat_a_touched_session() {
        let store = Store::open_memory().await.expect("store");
        seed(&store, "ses_a", 1_000).await;
        seed(&store, "ses_b", 2_000).await;
        seed(&store, "ses_c", 3_000).await;

        let first = store
            .list_sessions(SessionQuery {
                limit: 2,
                order: Order::Asc,
                ..Default::default()
            })
            .await
            .expect("first page");
        assert_eq!(ids(&first), vec!["ses_a", "ses_b"]);
        let next = first.cursor.next.expect("next cursor");

        // ses_a was already emitted, then it becomes active again.
        touch(&store, "ses_a", 4_000).await;

        let second = store
            .list_sessions(SessionQuery {
                cursor: Some(next),
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("second page");
        assert_eq!(
            ids(&second),
            vec!["ses_c"],
            "a visited row must not reappear in the same walk"
        );
        assert!(
            second.cursor.next.is_none(),
            "the walk ends at its frozen last row"
        );
    }

    /// A cursor written before the `snapshot` field existed must keep working and
    /// must not silently acquire a bound.
    #[tokio::test]
    async fn legacy_cursor_without_snapshot_still_pages() {
        let store = Store::open_memory().await.expect("store");
        seed(&store, "ses_a", 1_000).await;
        seed(&store, "ses_b", 2_000).await;
        seed(&store, "ses_c", 3_000).await;

        let legacy = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "id": "ses_b",
                "time": 2_000,
                "order": "asc",
                "direction": "next",
                "filters": {},
            }))
            .expect("encode legacy cursor"),
        );
        let page = store
            .list_sessions(SessionQuery {
                limit: 2,
                cursor: Some(legacy),
                ..Default::default()
            })
            .await
            .expect("legacy page");
        assert_eq!(ids(&page), vec!["ses_c"]);
        // A non-empty page reached through a cursor always offers a `previous`
        // cursor; it must stay legacy too, or the walk would silently acquire a
        // bound halfway through.
        let previous = page.cursor.previous.expect("previous cursor");
        assert!(
            decode(&previous).get("snapshot").is_none(),
            "a legacy walk keeps emitting legacy cursors"
        );
    }

    /// Both directions and the backward walk keep their ordering and cursors.
    #[tokio::test]
    async fn ascending_previous_and_descending_walks_still_work() {
        let store = Store::open_memory().await.expect("store");
        seed(&store, "ses_a", 1_000).await;
        seed(&store, "ses_b", 2_000).await;
        seed(&store, "ses_c", 3_000).await;

        let first = store
            .list_sessions(SessionQuery {
                limit: 2,
                order: Order::Asc,
                ..Default::default()
            })
            .await
            .expect("asc page");
        assert_eq!(ids(&first), vec!["ses_a", "ses_b"]);
        assert!(first.cursor.previous.is_none());
        let second = store
            .list_sessions(SessionQuery {
                cursor: first.cursor.next,
                ..Default::default()
            })
            .await
            .expect("next page");
        assert_eq!(ids(&second), vec!["ses_c"]);
        let back = store
            .list_sessions(SessionQuery {
                cursor: second.cursor.previous,
                ..Default::default()
            })
            .await
            .expect("previous page");
        assert_eq!(ids(&back), vec!["ses_a", "ses_b"]);

        let desc = store
            .list_sessions(SessionQuery {
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("desc page");
        assert_eq!(ids(&desc), vec!["ses_c", "ses_b"]);
        let tail = store
            .list_sessions(SessionQuery {
                cursor: desc.cursor.next,
                ..Default::default()
            })
            .await
            .expect("desc tail");
        assert_eq!(ids(&tail), vec!["ses_a"]);
    }

    /// Pins the documented residual hazard: the bound freezes the walk but does NOT
    /// restore a row that was touched to a time still inside the snapshot. Moving a
    /// row home needs an immutable sort key, which is a schema/UX decision.
    #[tokio::test]
    async fn row_touched_inside_the_snapshot_can_still_move_across_the_cursor() {
        let store = Store::open_memory().await.expect("store");
        seed(&store, "ses_a", 1_000).await;
        seed(&store, "ses_b", 2_000).await;
        seed(&store, "ses_c", 3_000).await;

        let first = store
            .list_sessions(SessionQuery {
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("first page");
        assert_eq!(ids(&first), vec!["ses_c", "ses_b"]);
        let next = first.cursor.next.expect("next cursor");

        // 2_500 is <= the frozen snapshot (3_000), so the bound does not cover it.
        touch(&store, "ses_a", 2_500).await;

        let second = store
            .list_sessions(SessionQuery {
                cursor: Some(next),
                limit: 2,
                ..Default::default()
            })
            .await
            .expect("second page");
        assert!(
            ids(&second).is_empty(),
            "documented residual: the bound cannot un-skip a row that moved inside it"
        );
        let fresh = store
            .list_sessions(SessionQuery {
                limit: 5,
                ..Default::default()
            })
            .await
            .expect("fresh walk");
        assert_eq!(
            ids(&fresh),
            vec!["ses_c", "ses_a", "ses_b"],
            "a new walk sees the current order"
        );
    }
}
