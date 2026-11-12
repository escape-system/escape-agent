//! The keyset list query over `session_v2`.

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
            conditions.push("title LIKE ?".to_owned());
            values.push(SqlValue::Text(format!("%{search}%")));
        }
        match &anchor.filters.parent {
            Some(Some(parent)) => {
                conditions.push("parent_id = ?".to_owned());
                values.push(SqlValue::Text(parent.clone()));
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
            conditions.push("directory = ?".to_owned());
            values.push(SqlValue::Text(directory.clone()));
        }
        if let Some(project) = &anchor.filters.project {
            conditions.push("project_id = ?".to_owned());
            values.push(SqlValue::Text(project.clone()));
        }
        if let Some(subpath) = &anchor.filters.subpath {
            conditions.push("path = ?".to_owned());
            values.push(SqlValue::Text(subpath.clone()));
        }
        if let Some((time, id)) = &anchor.position {
            let comparator = if effective == Order::Asc { ">" } else { "<" };
            conditions.push(format!(
                "(time_updated {comparator} ? OR (time_updated = ? AND id {comparator} ?))"
            ));
            values.push(SqlValue::Integer(*time));
            values.push(SqlValue::Integer(*time));
            values.push(SqlValue::Text(id.clone()));
        }

        let mut sql = format!("SELECT {SESSION_COLUMNS} FROM session_v2");
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        write!(
            sql,
            " ORDER BY time_updated {}, id {} LIMIT ?",
            effective.sql(),
            effective.sql()
        )
        .expect("writing to a String cannot fail");
        values.push(SqlValue::Integer(
            i64::try_from(query.limit + 1).unwrap_or(i64::MAX),
        ));

        let rows = self
            .read(move |connection| {
                let mut statement = connection.prepare(&sql)?;
                let mut mapped = statement.query(params_from_iter(values.iter()))?;
                let mut items: Vec<(i64, String, Value)> = Vec::new();
                while let Some(row) = mapped.next()? {
                    let id: String = row.get(0)?;
                    let time: i64 = row.get(26)?;
                    items.push((time, id, session_value(row)?));
                }
                Ok(items)
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
            has_more,
            &anchor.filters,
        )?;

        Ok(SessionPage {
            items: rows.into_iter().map(|(_, _, value)| value).collect(),
            cursor: page_cursor,
        })
    }
}
