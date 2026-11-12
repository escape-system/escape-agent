//! Permanent memory rows: FTS-backed retrieval, deduplicating writes and usage
//! accounting.
//!
//! Scoring happens in SQLite. `bm25(memory_fts)` returns negative numbers where
//! more negative means better, so the score uses `-bm25` with the weights from
//! `docs/06-memory.md`:
//!
//! `score = -bm25 + importance * W_IMPORTANCE + recency * W_RECENCY + tag_overlap * W_TAG_OVERLAP`
//!
//! `recency = 1 / (1 + age_days / RECENCY_SCALE_DAYS)` with `age_days` measured
//! from `last_used ?? updated` in `MS_PER_DAY` units. `tag_overlap` counts
//! stored tags also requested by the query. The constants live below; the SQL is
//! assembled in [`score_expression`], the single construction site.

use std::collections::HashSet;

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::db::{DEFAULT_LIMIT, Store, in_tx, now_ms};
use crate::error::StoreError;

/// Scopes accepted on write.
const SCOPES: [&str; 4] = ["global", "project", "agent", "session"];
const SEARCH_DEFAULT: usize = 8;
const SEARCH_CAP: usize = 50;
const LIST_CAP: usize = 200;
const PINNED_LIMIT: u8 = 3;
const MAX_QUERY_TOKENS: usize = 32;
const MAX_TAG_COUNT: usize = 32;

/// Time unit of `memory.updated` / `memory.last_used` (epoch milliseconds),
/// bound as a REAL divisor in the recency expression.
const MS_PER_DAY: f64 = 86_400_000.0;
/// Scale of the recency term in days (see the module doc): the recency term is
/// halved once a memory is this old, so memories stay relevant for months
/// instead of days.
const RECENCY_SCALE_DAYS: f64 = 30.0;
/// Weight of the stored `importance` (1..=5) in the hybrid score.
const W_IMPORTANCE: f64 = 0.35;
/// Weight of the recency term in the hybrid score.
const W_RECENCY: f64 = 0.25;
/// Weight of the tag-overlap count in the hybrid score.
const W_TAG_OVERLAP: f64 = 0.20;

pub(crate) const MEMORY_COLUMNS: &str = "id, scope, scope_id, kind, text, tags, importance, confidence, source, \
source_session, source_message, created, updated, last_used, uses";

/// A memory to insert, or to fold into an existing row with the same
/// `(scope, scope_id, hash)`.
#[derive(Debug, Clone)]
pub struct NewMemory {
    /// Wire memory id (`mem_…`); ignored when an equivalent row already exists.
    pub id: String,
    /// `global`, `project`, `agent` or `session`.
    pub scope: String,
    /// Project / agent / session id, absent for `global`.
    pub scope_id: Option<String>,
    /// `fact`, `decision`, `preference`, `pattern` or `gotcha`.
    pub kind: String,
    /// Memory text; normalized only for hashing.
    pub text: String,
    /// Free-form tags.
    pub tags: Vec<String>,
    /// 1..=5; out-of-range values are clamped.
    pub importance: u8,
    /// 0.0..=1.0; out-of-range values are clamped.
    pub confidence: f32,
    /// `user`, `agent`, `scribe` or `import`.
    pub source: String,
    /// Session that produced the memory.
    pub source_session: Option<String>,
    /// Message that produced the memory.
    pub source_message: Option<String>,
}

/// Scope filters plus retrieval options for [`Store::search_memories`] and
/// [`Store::list_memories`].
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    /// Visible scopes, e.g. `[("global", None), ("project", Some(id))]`. An
    /// empty list means no scope restriction.
    pub scopes: Vec<(String, Option<String>)>,
    /// Free-text query for FTS matching. Ignored by [`Store::list_memories`].
    pub query: Option<String>,
    /// Restrict to these kinds.
    pub kinds: Vec<String>,
    /// Require at least one of these tags.
    pub tags: Vec<String>,
    /// Page size; `0` uses the default (8 for search, 50 for list).
    pub limit: usize,
    /// When set, rows with at least this importance are always returned (up to
    /// three) even when the query does not match them.
    pub pinned_min_importance: Option<u8>,
}

/// One ranked search result.
#[derive(Debug, Clone)]
pub struct MemoryHit {
    /// Memory id.
    pub id: String,
    /// Memory kind.
    pub kind: String,
    /// Memory text.
    pub text: String,
    /// Stored tags.
    pub tags: Vec<String>,
    /// Stored importance.
    pub importance: u8,
    /// Hybrid score; higher is better.
    pub score: f64,
}

impl Store {
    /// Writes a memory, folding duplicates into the existing row.
    ///
    /// The identity is `(scope, scope_id, hash)` where `hash` is SHA-256 of the
    /// normalized text (trimmed, whitespace-collapsed, lowercased). An
    /// equivalent write updates importance, confidence and `updated`, clears a
    /// soft delete, and returns the existing row (its id wins). The exception is
    /// a `scribe` write of a forgotten row: the scribe must not undo an explicit
    /// `memory_forget`, so that write fails with [`StoreError::Conflict`]
    /// instead of returning a row every read keeps filtering out.
    #[allow(clippy::too_many_lines)]
    pub async fn write_memory(&self, new: NewMemory) -> Result<Value, StoreError> {
        if !SCOPES.contains(&new.scope.as_str()) {
            return Err(StoreError::Invalid(format!(
                "unknown memory scope {:?}",
                new.scope
            )));
        }
        if new.kind.trim().is_empty() {
            return Err(StoreError::Invalid(
                "memory kind must not be empty".to_owned(),
            ));
        }
        let normalized = normalize_text(&new.text);
        if normalized.is_empty() {
            return Err(StoreError::Invalid(
                "memory text must not be empty".to_owned(),
            ));
        }
        let hash = hash_text(&normalized);
        let tags = encode_tags(&new.tags)?;
        let importance = i64::from(new.importance.clamp(1, 5));
        let confidence = if new.confidence.is_finite() {
            f64::from(new.confidence.clamp(0.0, 1.0))
        } else {
            0.7
        };
        let text = new.text.trim().to_owned();
        let time = now_ms();
        let NewMemory {
            id,
            scope,
            scope_id,
            kind,
            source,
            source_session,
            source_message,
            ..
        } = new;
        let id_for_read = id.clone();
        let stored_id = self
            .write(move |connection| {
                in_tx(connection, |transaction| {
                    let existing: Option<(String, i64)> = transaction
                        .query_row(
                            "SELECT id, deleted FROM memory
                             WHERE scope = ?1 AND coalesce(scope_id, '') = coalesce(?2, '')
                               AND hash = ?3",
                            params![&scope, &scope_id, &hash],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    if let Some((existing, is_deleted)) = existing {
                        if is_deleted != 0 {
                            // A forgotten row is invisible to every read. Only
                            // an explicit rewrite may restore it; a scribe retry
                            // must not undo the forget or claim it succeeded.
                            if source.as_str() == "scribe" {
                                return Err(StoreError::Conflict(format!(
                                    "memory {existing} was forgotten; the scribe must not recreate it"
                                )));
                            }
                        }
                        transaction.execute(
                            "UPDATE memory
                             SET importance = ?2, confidence = ?3, updated = ?4, deleted = 0
                             WHERE id = ?1",
                            params![&existing, importance, confidence, time],
                        )?;
                        return Ok(existing);
                    }
                    let duplicate: Option<i64> = transaction
                        .query_row("SELECT 1 FROM memory WHERE id = ?1", [&id], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    if duplicate.is_some() {
                        return Err(StoreError::Conflict(format!(
                            "memory {id} already exists with different text"
                        )));
                    }
                    transaction.execute(
                        "INSERT INTO memory
                            (id, scope, scope_id, kind, text, tags, importance, confidence,
                             source, source_session, source_message, hash, created, updated,
                             uses, deleted)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, 0, 0)",
                        params![
                            &id,
                            &scope,
                            &scope_id,
                            &kind,
                            &text,
                            &tags,
                            importance,
                            confidence,
                            &source,
                            &source_session,
                            &source_message,
                            &hash,
                            time
                        ],
                    )?;
                    Ok(id)
                })
            })
            .await?;
        self.memory_row(&stored_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("memory {id_for_read}")))
    }

    /// Searches memories and returns them ranked by the hybrid score.
    ///
    /// Soft-deleted rows are excluded, the limit is capped at 50, and pinned
    /// rows (`importance >= pinned_min_importance`) are ordered first so up to
    /// three of them survive the limit even without a query match.
    pub async fn search_memories(&self, query: MemoryQuery) -> Result<Vec<MemoryHit>, StoreError> {
        let limit = if query.limit == 0 {
            SEARCH_DEFAULT
        } else {
            query.limit.min(SEARCH_CAP)
        };
        let (sql, values) = search_statement(&query, limit, now_ms());

        self.read(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut hits = Vec::new();
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let text: String = row.get(2)?;
                let tags: String = row.get(3)?;
                let importance: i64 = row.get(4)?;
                let score: f64 = row.get(5)?;
                hits.push(MemoryHit {
                    id,
                    kind,
                    text,
                    tags: decode_tags(&tags)?,
                    importance: u8::try_from(importance).unwrap_or_default(),
                    score,
                });
            }
            Ok(hits)
        })
        .await
    }

    /// Lists memories by importance and recency. `query` is ignored here.
    pub async fn list_memories(&self, query: MemoryQuery) -> Result<Vec<Value>, StoreError> {
        let limit = if query.limit == 0 {
            DEFAULT_LIMIT
        } else {
            query.limit.min(LIST_CAP)
        };
        let filters = build_filters(&query);
        let mut values = filters.params;
        values.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        let sql = format!(
            "SELECT {MEMORY_COLUMNS} FROM memory m
             WHERE m.deleted = 0{filter_clause}
             ORDER BY m.importance DESC, m.updated DESC, m.id ASC
             LIMIT ?",
            filter_clause = filters.clause,
        );
        self.read(move |connection| {
            let mut statement = connection.prepare(&sql)?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut items = Vec::new();
            while let Some(row) = rows.next()? {
                items.push(memory_value(row)?);
            }
            Ok(items)
        })
        .await
    }

    /// Soft-deletes a memory. Returns whether a live row was changed.
    pub async fn forget_memory(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_owned();
        let time = now_ms();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let changed = transaction.execute(
                    "UPDATE memory SET deleted = 1, updated = ?2
                     WHERE id = ?1 AND deleted = 0",
                    params![&id, time],
                )?;
                Ok(changed > 0)
            })
        })
        .await
    }

    /// Bumps `uses` and `last_used` for the given ids in one statement.
    ///
    /// Forgotten rows are left alone: a memory retrieved before a `forget` and
    /// touched at turn end must not keep accruing usage. Unknown ids are a
    /// silent skip — a session-scoped memory may legitimately disappear between
    /// retrieval and the touch, so that is not an error.
    pub async fn touch_memories(&self, ids: &[String], now: i64) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        self.write(move |connection| {
            in_tx(connection, |transaction| {
                let placeholders = vec!["?"; ids.len()].join(", ");
                let sql = format!(
                    "UPDATE memory SET uses = coalesce(uses, 0) + 1, last_used = ?1
                     WHERE deleted = 0 AND id IN ({placeholders})"
                );
                let mut values: Vec<SqlValue> = Vec::with_capacity(ids.len() + 1);
                values.push(SqlValue::Integer(now));
                values.extend(ids.iter().map(|id| SqlValue::Text(id.clone())));
                transaction.execute(&sql, params_from_iter(values.iter()))?;
                Ok(())
            })
        })
        .await
    }

    async fn memory_row(&self, id: &str) -> Result<Option<Value>, StoreError> {
        let id = id.to_owned();
        self.read(move |connection| {
            let sql = format!("SELECT {MEMORY_COLUMNS} FROM memory WHERE id = ?1");
            let row = connection
                .query_row(&sql, [&id], |row| Ok(memory_value(row)))
                .optional()?;
            row.transpose()
        })
        .await
    }
}

/// The hybrid-score SQL, built once so the FTS branch and the plain-ranked
/// branch cannot drift apart.
///
/// `score = bm25 + importance * W_IMPORTANCE + recency * W_RECENCY
///          + tag_overlap * W_TAG_OVERLAP`
///
/// with `recency = 1 / (1 + age_days / RECENCY_SCALE_DAYS)` and
/// `age_days = max(0, now - coalesce(last_used, updated)) / MS_PER_DAY`. The
/// `max(0, …)` keeps a clock step backwards from yielding a negative age and
/// therefore a recency above 1.
///
/// Every policy number is a bound `?` parameter (never a literal inside the
/// SQL), so the constants above are the single definition. `?` order in the
/// returned fragment: `W_IMPORTANCE`, `now`, `MS_PER_DAY`, `RECENCY_SCALE_DAYS`,
/// `W_RECENCY`, the overlap-tag values (if any), `W_TAG_OVERLAP`.
struct ScoreExpression {
    sql: String,
    params: Vec<SqlValue>,
}

fn score_expression(tags: &[String], now: i64, bm25: Option<&str>) -> ScoreExpression {
    let (overlap_sql, overlap_params) = overlap_expression(tags);
    // `1.0` is the constant term of the documented `1 / (1 + x)` recency curve,
    // not a tunable policy number. The divisors are bound as REAL so the integer
    // epoch-millisecond difference yields a fractional day count instead of
    // SQLite's integer division.
    let weighted = format!(
        "(m.importance * ?)
         + (1.0 / (1.0 + max(0, ? - coalesce(m.last_used, m.updated)) / ? / ?)) * ?
         + ({overlap_sql}) * ?"
    );
    let sql = match bm25 {
        Some(term) => format!("({term} + {weighted})"),
        None => format!("({weighted})"),
    };
    let mut params = vec![
        SqlValue::Real(W_IMPORTANCE),
        SqlValue::Integer(now),
        SqlValue::Real(MS_PER_DAY),
        SqlValue::Real(RECENCY_SCALE_DAYS),
        SqlValue::Real(W_RECENCY),
    ];
    params.extend(overlap_params);
    params.push(SqlValue::Real(W_TAG_OVERLAP));
    ScoreExpression { sql, params }
}

fn search_statement(query: &MemoryQuery, limit: usize, now: i64) -> (String, Vec<SqlValue>) {
    let match_query = query.query.as_deref().and_then(fts_query);
    let pinned = query
        .pinned_min_importance
        .map(|value| i64::from(value.clamp(1, 5)));
    let filters = build_filters(query);
    let pinned_flag = pinned.map_or_else(
        || "0".to_owned(),
        |_| "CASE WHEN m.importance >= ? THEN 1 ELSE 0 END".to_owned(),
    );
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut values: Vec<SqlValue> = Vec::new();
    let sql = if let Some(match_query) = match_query {
        values.push(SqlValue::Text(match_query));
        if let Some(threshold) = pinned {
            values.push(SqlValue::Integer(threshold));
        }
        let score = score_expression(&query.tags, now, Some("coalesce(h.bm, 0.0)"));
        values.extend(score.params);
        values.extend(filters.params);
        if let Some(threshold) = pinned {
            values.push(SqlValue::Integer(threshold));
        }
        values.push(SqlValue::Integer(limit));
        let pinned_gate = if pinned.is_some() {
            " OR m.importance >= ?"
        } else {
            ""
        };
        format!(
            "WITH hits AS (
                 SELECT rowid AS rid, -bm25(memory_fts) AS bm
                 FROM memory_fts WHERE memory_fts MATCH ?
             ), scored AS (
                 SELECT m.id AS id, m.kind AS kind, m.text AS text, m.tags AS tags,
                        m.importance AS importance, m.updated AS updated,
                        h.rid AS hit_rid, {pinned_flag} AS pinned_flag,
                        {score_sql} AS score
                 FROM memory m
                 LEFT JOIN hits h ON h.rid = m.rowid
                 WHERE m.deleted = 0{filter_clause}
                   AND (h.rid IS NOT NULL{pinned_gate})
             ), ranked AS (
                 SELECT id, kind, text, tags, importance, updated, hit_rid, pinned_flag, score,
                        CASE WHEN hit_rid IS NULL
                             THEN ROW_NUMBER() OVER (
                                 PARTITION BY (hit_rid IS NULL)
                                 ORDER BY importance DESC, updated DESC)
                             ELSE 0 END AS pinned_rank
                 FROM scored
             )
             SELECT id, kind, text, tags, importance, score
             FROM ranked
             WHERE hit_rid IS NOT NULL OR pinned_rank <= {PINNED_LIMIT}
             ORDER BY pinned_flag DESC, score DESC, importance DESC, updated DESC
             LIMIT ?",
            filter_clause = filters.clause,
            score_sql = score.sql,
        )
    } else {
        let score = score_expression(&query.tags, now, None);
        values.extend(score.params);
        values.extend(filters.params);
        values.push(SqlValue::Integer(limit));
        format!(
            "SELECT m.id, m.kind, m.text, m.tags, m.importance, {score_sql} AS score
             FROM memory m
             WHERE m.deleted = 0{filter_clause}
             ORDER BY score DESC, m.importance DESC, m.updated DESC
             LIMIT ?",
            filter_clause = filters.clause,
            score_sql = score.sql,
        )
    };
    (sql, values)
}

struct Filters {
    clause: String,
    params: Vec<SqlValue>,
}

fn build_filters(query: &MemoryQuery) -> Filters {
    let mut clause = String::new();
    let mut params: Vec<SqlValue> = Vec::new();

    if !query.scopes.is_empty() {
        let mut parts = Vec::with_capacity(query.scopes.len());
        for (scope, scope_id) in &query.scopes {
            if let Some(scope_id) = scope_id {
                parts.push("(m.scope = ? AND m.scope_id = ?)");
                params.push(SqlValue::Text(scope.clone()));
                params.push(SqlValue::Text(scope_id.clone()));
            } else {
                parts.push("m.scope = ?");
                params.push(SqlValue::Text(scope.clone()));
            }
        }
        clause.push_str(" AND (");
        clause.push_str(&parts.join(" OR "));
        clause.push(')');
    }

    if !query.kinds.is_empty() {
        let placeholders = vec!["?"; query.kinds.len()].join(", ");
        clause.push_str(" AND m.kind IN (");
        clause.push_str(&placeholders);
        clause.push(')');
        params.extend(query.kinds.iter().map(|kind| SqlValue::Text(kind.clone())));
    }

    if !query.tags.is_empty() {
        let placeholders = vec!["?"; query.tags.len()].join(", ");
        clause.push_str(" AND EXISTS (SELECT 1 FROM json_each(m.tags) AS j WHERE j.value IN (");
        clause.push_str(&placeholders);
        clause.push_str("))");
        params.extend(query.tags.iter().map(|tag| SqlValue::Text(tag.clone())));
    }

    Filters { clause, params }
}

/// SQL expression counting stored tags that also appear in `tags`.
fn overlap_expression(tags: &[String]) -> (String, Vec<SqlValue>) {
    if tags.is_empty() {
        return ("0".to_owned(), Vec::new());
    }
    let placeholders = vec!["?"; tags.len()].join(", ");
    let sql =
        format!("(SELECT count(*) FROM json_each(m.tags) AS t WHERE t.value IN ({placeholders}))");
    let params = tags.iter().map(|tag| SqlValue::Text(tag.clone())).collect();
    (sql, params)
}

fn memory_value(row: &Row<'_>) -> Result<Value, StoreError> {
    let id: String = row.get(0)?;
    let scope: String = row.get(1)?;
    let scope_id: Option<String> = row.get(2)?;
    let kind: String = row.get(3)?;
    let text: String = row.get(4)?;
    let tags: String = row.get(5)?;
    let importance: i64 = row.get(6)?;
    let confidence: f64 = row.get(7)?;
    let source: String = row.get(8)?;
    let source_session: Option<String> = row.get(9)?;
    let source_message: Option<String> = row.get(10)?;
    let created: i64 = row.get(11)?;
    let updated: i64 = row.get(12)?;
    let last_used: Option<i64> = row.get(13)?;
    let uses: i64 = row.get(14)?;

    let mut map = Map::new();
    map.insert("id".to_owned(), Value::String(id));
    map.insert("scope".to_owned(), Value::String(scope));
    if let Some(scope_id) = scope_id {
        map.insert("scopeID".to_owned(), Value::String(scope_id));
    }
    map.insert("kind".to_owned(), Value::String(kind));
    map.insert("text".to_owned(), Value::String(text));
    map.insert(
        "tags".to_owned(),
        Value::Array(decode_tags(&tags)?.into_iter().map(Value::String).collect()),
    );
    map.insert(
        "importance".to_owned(),
        json!(u8::try_from(importance).unwrap_or_default()),
    );
    map.insert("confidence".to_owned(), json!(confidence));
    map.insert("source".to_owned(), Value::String(source));
    if let Some(source_session) = source_session {
        map.insert("sourceSession".to_owned(), Value::String(source_session));
    }
    if let Some(source_message) = source_message {
        map.insert("sourceMessage".to_owned(), Value::String(source_message));
    }
    map.insert("created".to_owned(), json!(created));
    map.insert("updated".to_owned(), json!(updated));
    if let Some(last_used) = last_used {
        map.insert("lastUsed".to_owned(), json!(last_used));
    }
    map.insert("uses".to_owned(), json!(uses));
    Ok(Value::Object(map))
}

/// Trims, collapses whitespace and lowercases for hashing.
fn normalize_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn hash_text(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

fn encode_tags(tags: &[String]) -> Result<String, StoreError> {
    let mut seen = HashSet::new();
    let mut cleaned: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() || !seen.insert(tag.to_owned()) {
            continue;
        }
        cleaned.push(tag.to_owned());
        if cleaned.len() >= MAX_TAG_COUNT {
            break;
        }
    }
    Ok(serde_json::to_string(&cleaned)?)
}

fn decode_tags(raw: &str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str(raw)
        .map_err(|error| StoreError::Corrupt(format!("memory tags are invalid: {error}")))
}

/// Turns free text into a safe FTS5 query: quoted alphanumeric tokens joined by
/// `OR`. Returns `None` when nothing usable remains.
fn fts_query(raw: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let mut tokens = Vec::new();
    for token in raw.split(|character: char| !(character.is_alphanumeric() || character == '_')) {
        if token.is_empty() {
            continue;
        }
        let token: String = token.chars().take(64).collect();
        if !seen.insert(token.clone()) {
            continue;
        }
        tokens.push(format!("\"{token}\""));
        if tokens.len() >= MAX_QUERY_TOKENS {
            break;
        }
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}
