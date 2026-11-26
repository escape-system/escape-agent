//! Context box tools and the `oc/boxes` prompt index (`docs/25-context-boxes.md`).
//!
//! Boxes are addressed by key inside one session tree. The model sees a
//! compact index in the system prefix and loads content on demand with
//! `box_read`; nothing but the index is ever inlined. Writes are
//! create-or-update, so a retry never duplicates a box.
//!
//! `compact_context` never compacts inside a tool call: it enqueues a
//! `compaction` inbox row and wakes the actor, which runs it at the next
//! boundary. That keeps the current turn's transcript intact while the request
//! is made.

use std::fmt::Write as _;
use std::sync::Arc;

use oc_config::Effect as ConfigEffect;
use oc_store::{
    BOX_SCOPES, BoxMetaPatch, BoxQuery, BoxSearchQuery, Delivery, EditBox, NewInbox, StoreError,
    WriteBox,
};
use oc_tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use oc_types::{MessageId, now_millis};
use serde_json::{Map, Value, json};

use crate::runtime::Runtime;

/// Maximum characters accepted by one `box_write` call. The store's hard cap
/// is higher so an append can grow a box across several calls.
pub const MAX_BOX_WRITE_CHARS: usize = 60_000;

/// Characters returned by one `box_read` call.
pub const BOX_READ_CHUNK: usize = 20_000;

/// Boxes rendered in the prompt index. Index queries fetch one row past this
/// limit so the renderer can prove that more boxes exist (the store caps one
/// page at 200 rows, so the extra row is always available).
pub const BOX_INDEX_MAX: usize = 100;

/// Rows the index query fetches: one past [`BOX_INDEX_MAX`] so the renderer can
/// prove that more boxes exist instead of truncating silently. The store clamps
/// one page at 200 rows, so the probe row is always available.
pub(crate) const BOX_INDEX_QUERY_MAX: usize = BOX_INDEX_MAX + 1;

/// Build the box tool set for one runtime.
#[derive(Clone, Debug)]
pub struct BoxTools {
    runtime: Runtime,
}

impl BoxTools {
    #[must_use]
    pub fn new(runtime: Runtime) -> Self {
        Self { runtime }
    }

    /// Registry entries: `box_list`, `box_read`, `box_write`, `box_edit`,
    /// `box_search`, `box_meta`, `box_delete` and `compact_context`.
    #[must_use]
    pub fn registry(&self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(BoxListTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(BoxReadTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(BoxWriteTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(BoxEditTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(BoxSearchTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(BoxMetaTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(BoxDeleteTool {
            runtime: self.runtime.clone(),
        }));
        registry.register(Arc::new(CompactContextTool {
            runtime: self.runtime.clone(),
        }));
        registry
    }
}

/// Render the cache-stable `<boxes>` index. Content is never included: the
/// model reads it through `box_read` when it needs it.
///
/// The block only depends on the live box set (key, kind, size, description),
/// so it changes when a box is added, rewritten with a different size/kind, or
/// expired — not on every turn.
#[must_use]
pub fn render_box_index(root_id: &str, boxes: &[Value], limit: usize) -> String {
    let mut live: Vec<&Value> = boxes
        .iter()
        .filter(|value| value.get("timeExpired").is_none())
        .collect();
    if live.is_empty() {
        return String::new();
    }
    live.sort_by(|left, right| {
        left.get("key")
            .and_then(Value::as_str)
            .cmp(&right.get("key").and_then(Value::as_str))
    });
    let mut out = String::with_capacity(128 + live.len() * 64);
    let _ = writeln!(out, "<boxes root=\"{}\">", escape_attr(root_id));
    for (index, value) in live.iter().enumerate() {
        if index >= limit {
            // The caller fetches one row past `limit`, so the extra row proves
            // more exist; the true total may be higher, hence "at least".
            let _ = writeln!(
                out,
                "- … {} more boxes; call box_list",
                live.len().saturating_sub(limit)
            );
            break;
        }
        let key = value.get("key").and_then(Value::as_str).unwrap_or_default();
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("stable");
        let scope = value.get("scope").and_then(Value::as_str).unwrap_or("tree");
        let badge = if scope == "session" {
            format!("[{kind}, session]")
        } else {
            format!("[{kind}]")
        };
        let size = value.get("size").and_then(Value::as_u64).unwrap_or(0);
        let note = value
            .get("description")
            .or_else(|| value.get("title"))
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|note| !note.is_empty());
        match note {
            Some(note) => {
                let _ = writeln!(out, "- {badge} {key} ({size} chars) — {note}");
            }
            None => {
                let _ = writeln!(out, "- {badge} {key} ({size} chars)");
            }
        }
    }
    out.push_str("</boxes>");
    out
}

/// One-line, length-bounded description for the index.
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(160).collect()
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Content-free projection of one store box row, used for every tool metadata
/// payload. A box body is returned only by `box_read`'s `output`; metadata is
/// attached to events and transcripts, so it must never carry the body.
///
/// Fields: `id`, `key`, `kind`, `title`, `description` (omitted when absent),
/// `size`, `revision`, `timeCreated`, `timeUpdated`, `rootID`, `ownerSession`
/// and `tags` verbatim. `content` and `timeExpired` are intentionally absent.
fn box_summary(value: &Value) -> Value {
    const FIELDS: [&str; 12] = [
        "id",
        "key",
        "scope",
        "kind",
        "title",
        "description",
        "size",
        "revision",
        "timeCreated",
        "timeUpdated",
        "rootID",
        "ownerSession",
    ];
    let mut map = Map::new();
    for field in FIELDS {
        if let Some(field_value) = value.get(field) {
            map.insert(field.to_owned(), field_value.clone());
        }
    }
    if let Some(tags) = value.get("tags") {
        map.insert("tags".to_owned(), tags.clone());
    }
    Value::Object(map)
}

impl Runtime {
    /// Root session id for a tool call. Every box operation is scoped to the
    /// tree, so a subagent reads and writes the same boxes as its root.
    pub(crate) async fn box_root(&self, session: &str) -> Result<String, ToolError> {
        match self.inner.config.store.session_node(session).await {
            Ok(Some(node)) => Ok(node.root_id),
            Ok(None) => Ok(session.to_owned()),
            Err(error) => Err(store_error(&error)),
        }
    }

    /// Live boxes visible to `session_id` (`tree` + caller `session` boxes).
    pub(crate) async fn box_index_rows_for(
        &self,
        root_id: &str,
        session_id: &str,
    ) -> Result<Vec<Value>, ToolError> {
        self.inner
            .config
            .store
            .list_boxes(BoxQuery {
                root_id: root_id.to_owned(),
                session_id: Some(session_id.to_owned()),
                limit: BOX_INDEX_QUERY_MAX,
                ..BoxQuery::default()
            })
            .await
            .map(|page| page.items)
            .map_err(|error| store_error(&error))
    }
}

struct BoxListTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxListTool {
    fn name(&self) -> &'static str {
        "box_list"
    }

    fn description(&self) -> &'static str {
        "List session-scoped context boxes visible to this session (`tree` shared across the tree, or `session` private to this session). Boxes belong only to the current session tree and never carry over to future projects (`memory_*` handles cross-session knowledge). Content is omitted; call box_read or box_search."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Filter by visibility scope (`tree` shared across tree, `session` private to this session)."
                },
                "kind": {
                    "type": "string",
                    "enum": ["stable", "temporary"],
                    "description": "Only boxes of this kind."
                },
                "prefix": {
                    "type": "string",
                    "description": "Only boxes whose key starts with this prefix."
                },
                "tag": {
                    "type": "string",
                    "description": "Only boxes tagged with this exact tag."
                },
                "cursor": {
                    "type": "string",
                    "description": "Pagination cursor returned as `nextCursor` by an earlier box_list call."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Maximum boxes to return (default 100, cap 200)."
                },
                "include_expired": {
                    "type": "boolean",
                    "description": "Include temporary boxes expired by a compaction (default false)."
                }
            }
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let kind = optional_enum(
            &input,
            "kind",
            &oc_store::BOX_KINDS,
            "`kind` must be `stable` or `temporary`",
        )?;
        let include_expired = optional_bool(&input, "include_expired")?.unwrap_or(false);
        let prefix = optional_str(&input, "prefix");
        let tag = optional_str(&input, "tag");
        let cursor = optional_str(&input, "cursor");
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(BOX_INDEX_MAX, |value| {
                usize::try_from(value).unwrap_or(BOX_INDEX_MAX)
            })
            .clamp(1, 200);
        // Every box is tree-anchored, `session`-scoped rows belong solely to
        // `ctx.session_id`, and `box:read` visibility is evaluated per key so
        // denied boxes never leak through listings.
        let session = load_session_info(&self.runtime, &ctx.session_id).await?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let page = self
            .runtime
            .inner
            .config
            .store
            .list_boxes(BoxQuery {
                root_id: root.clone(),
                scope,
                session_id: Some(ctx.session_id.clone()),
                kind,
                prefix,
                tag,
                cursor,
                include_expired,
                limit,
            })
            .await
            .map_err(|error| store_error(&error))?;
        let visible: Vec<&Value> = page
            .items
            .iter()
            .filter(|item| {
                let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
                self.runtime
                    .stored_session_effect(&session, "box:read", key)
                    != ConfigEffect::Deny
            })
            .collect();
        let summaries: Vec<Value> = visible.iter().map(|item| box_summary(item)).collect();
        let out =
            format_box_list_output(&root, &visible, page.has_more, page.next_cursor.as_deref());
        let mut metadata = json!({
            "boxes": summaries,
            "hasMore": page.has_more,
        });
        if let Some(next_cursor) = page.next_cursor {
            metadata["nextCursor"] = Value::String(next_cursor);
        }
        Ok(ToolResult::text(out).with_metadata(metadata))
    }
}

fn format_box_list_output(
    root: &str,
    visible: &[&Value],
    has_more: bool,
    next_cursor: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "root {root}: {} box(es)", visible.len());
    if has_more {
        if let Some(next_cursor) = next_cursor {
            let _ = writeln!(
                out,
                "(more boxes exist than are listed; pass cursor={next_cursor:?} to continue)"
            );
        } else {
            out.push_str("(more boxes exist than are listed)\n");
        }
    }
    for item in visible {
        let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
        let kind = item.get("kind").and_then(Value::as_str).unwrap_or("stable");
        let item_scope = item.get("scope").and_then(Value::as_str).unwrap_or("tree");
        let badge = if item_scope == "session" {
            format!("[{kind}, session]")
        } else {
            format!("[{kind}]")
        };
        let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
        let revision = item.get("revision").and_then(Value::as_i64).unwrap_or(1);
        let expired = item
            .get("timeExpired")
            .map_or(String::new(), |_| " [expired]".to_owned());
        let note = item
            .get("description")
            .or_else(|| item.get("title"))
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|note| !note.is_empty())
            .map(|note| format!(" — {note}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "- {badge} {key} ({size} chars, rev {revision}){expired}{note}"
        );
    }
    out
}

struct BoxReadTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxReadTool {
    fn name(&self) -> &'static str {
        "box_read"
    }

    fn description(&self) -> &'static str {
        "Read a context box by key. Checks caller `session` scope before shared `tree` scope unless `scope` is specified. Returns chunks of at most 20000 characters; use `offset`/`limit` or `tail=true` for larger boxes. Expired temporary boxes are hidden unless include_expired is set."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Box key inside this session tree."
                },
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Read specifically from `tree` or caller `session` scope."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Character offset of the first returned character (default 0)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum characters to return (default and cap 20000)."
                },
                "tail": {
                    "type": "boolean",
                    "description": "When true, read the last `limit` characters instead of starting from `offset`."
                },
                "include_expired": {
                    "type": "boolean",
                    "description": "Read a temporary box that a compaction expired."
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let key = required_str(&input, "key")?;
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let include_expired = optional_bool(&input, "include_expired")?.unwrap_or(false);
        let tail = optional_bool(&input, "tail")?.unwrap_or(false);
        let offset = input
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |value| usize::try_from(value).unwrap_or(usize::MAX));
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(BOX_READ_CHUNK, |value| {
                usize::try_from(value).unwrap_or(BOX_READ_CHUNK)
            })
            .clamp(1, BOX_READ_CHUNK);
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let Some(box_value) = self
            .runtime
            .inner
            .config
            .store
            .get_box_scoped(
                &root,
                scope.as_deref(),
                Some(&ctx.session_id),
                &key,
                include_expired,
            )
            .await
            .map_err(|error| store_error(&error))?
        else {
            let expired = self
                .runtime
                .inner
                .config
                .store
                .get_box_scoped(&root, scope.as_deref(), Some(&ctx.session_id), &key, true)
                .await
                .map_err(|error| store_error(&error))?
                .is_some();
            return Err(ToolError::new(if expired {
                format!(
                    "box `{key}` was expired by a compaction; set include_expired or revive with box_meta"
                )
            } else {
                format!("no box `{key}` in root {root}; call box_list")
            }));
        };
        let content = box_value
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let total = content.chars().count();
        let start = if tail {
            total.saturating_sub(limit)
        } else {
            offset.min(total)
        };
        let chunk: String = content.chars().skip(start).take(limit).collect();
        let end = start + chunk.chars().count();
        let kind = box_value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("stable");
        let box_scope = box_value
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("tree");
        let badge = if box_scope == "session" {
            format!("[{kind}, session]")
        } else {
            format!("[{kind}]")
        };
        let mut out =
            format!("box `{key}` {badge} {total} chars, showing {start}..{end}\n\n{chunk}");
        if end < total {
            let _ = write!(
                out,
                "\n\n[{} chars remain; call box_read with offset={end}]",
                total - end
            );
        }
        Ok(ToolResult::text(out).with_metadata(json!({
            "key": key,
            "scope": box_scope,
            "kind": kind,
            "size": total,
            "offset": start,
            "returned": end - start,
            "revision": box_value.get("revision").cloned().unwrap_or(Value::Null),
        })))
    }
}

struct BoxWriteTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxWriteTool {
    fn name(&self) -> &'static str {
        "box_write"
    }

    fn description(&self) -> &'static str {
        "Create, overwrite, or append to a session-only context box (working notes, plans, findings, compaction handoffs). Use `scope=\"tree\"` (default) to share across this session tree or `scope=\"session\"` for private scratchpad state that will not collide with sibling agents. For durable cross-session project facts, use `memory_write` instead. Omitting `kind` or `tags` on an update/append preserves existing values; `stable` survives compaction and `temporary` expires at the next one."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Box key: lowercase letters, digits, `.`, `_`, `-`; 1..64 chars."
                },
                "content": {
                    "type": "string",
                    "description": "Full content (or the appended text when append is true); at most 60000 characters per call."
                },
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Visibility: `tree` (default, shared with parent/subagents) or `session` (private to this session)."
                },
                "kind": {
                    "type": "string",
                    "enum": ["stable", "temporary"],
                    "description": "Lifetime; defaults to `stable` when creating, preserved on update/append when omitted."
                },
                "title": {
                    "type": "string",
                    "description": "Short display title kept when omitted on update."
                },
                "description": {
                    "type": "string",
                    "description": "One-line description shown in the prompt index."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Free-form tags (preserved on update/append when omitted)."
                },
                "append": {
                    "type": "boolean",
                    "description": "Append content to the existing box instead of replacing it."
                },
                "expected_revision": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optimistic lock: fail if the live box revision differs."
                }
            },
            "required": ["key", "content"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let key = required_str(&input, "key")?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("`content` is required"))?;
        if content.trim().is_empty() {
            return Err(ToolError::new(
                "`content` must not be empty or whitespace-only; write real text, call box_meta for metadata, or box_delete to remove",
            ));
        }
        if content.chars().count() > MAX_BOX_WRITE_CHARS {
            return Err(ToolError::new(format!(
                "one box_write call accepts at most {MAX_BOX_WRITE_CHARS} characters"
            )));
        }
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let kind = optional_enum(
            &input,
            "kind",
            &oc_store::BOX_KINDS,
            "`kind` must be `stable` or `temporary`",
        )?;
        let append = optional_bool(&input, "append")?.unwrap_or(false);
        let tags = optional_string_array(&input, "tags")?;
        let expected_revision = optional_i64(&input, "expected_revision")?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let written = self
            .runtime
            .inner
            .config
            .store
            .write_box_scoped(
                WriteBox {
                    root_id: root,
                    owner_session: ctx.session_id.clone(),
                    scope,
                    key: key.clone(),
                    title: optional_str(&input, "title"),
                    description: optional_str(&input, "description"),
                    kind,
                    tags,
                    content: content.to_owned(),
                    expected_revision,
                },
                append,
            )
            .await
            .map_err(|error| match error {
                StoreError::Invalid(message) => ToolError::new(message),
                other => store_error(&other),
            })?;
        let size = written.get("size").and_then(Value::as_u64).unwrap_or(0);
        let revision = written.get("revision").and_then(Value::as_i64).unwrap_or(1);
        let kind = written
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("stable");
        let box_scope = written
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("tree");
        Ok(ToolResult::text(format!(
            "Box `{key}` written [{kind}, {box_scope}], {size} chars, revision {revision}. The prompt index updates on the next turn."
        ))
        .with_metadata(json!({ "box": box_summary(&written) })))
    }
}

struct BoxEditTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxEditTool {
    fn name(&self) -> &'static str {
        "box_edit"
    }

    fn description(&self) -> &'static str {
        "Apply a targeted find-and-replace edit inside a live context box without rewriting the entire content. Fails if `find` is missing or ambiguous (unless `replace_all=true`) or on `expected_revision` mismatch."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Box key to edit."
                },
                "find": {
                    "type": "string",
                    "description": "Exact existing text to replace (must be non-empty)."
                },
                "replace": {
                    "type": "string",
                    "description": "Replacement text."
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence instead of requiring a unique match (default false)."
                },
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Optional explicit scope (`tree` or `session`); omits checks caller `session` then `tree`."
                },
                "expected_revision": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optimistic lock against current box revision."
                }
            },
            "required": ["key", "find", "replace"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let key = required_str(&input, "key")?;
        let find = input
            .get("find")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::new("`find` is required and must be non-empty"))?;
        let replace = input
            .get("replace")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new("`replace` must be a string"))?;
        let replace_all = optional_bool(&input, "replace_all")?.unwrap_or(false);
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let expected_revision = optional_i64(&input, "expected_revision")?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let updated = self
            .runtime
            .inner
            .config
            .store
            .edit_box(EditBox {
                root_id: root,
                owner_session: ctx.session_id.clone(),
                scope,
                key: key.clone(),
                find: find.to_owned(),
                replace: replace.to_owned(),
                replace_all,
                expected_revision,
            })
            .await
            .map_err(|error| match error {
                StoreError::Invalid(message) | StoreError::NotFound(message) => {
                    ToolError::new(message)
                }
                other => store_error(&other),
            })?;
        let size = updated.get("size").and_then(Value::as_u64).unwrap_or(0);
        let revision = updated.get("revision").and_then(Value::as_i64).unwrap_or(1);
        let box_scope = updated
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("tree");
        Ok(ToolResult::text(format!(
            "Box `{key}` [{box_scope}] edited: {size} chars, revision {revision}."
        ))
        .with_metadata(json!({ "box": box_summary(&updated) })))
    }
}

struct BoxSearchTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxSearchTool {
    fn name(&self) -> &'static str {
        "box_search"
    }

    fn description(&self) -> &'static str {
        "Search context boxes visible to this session (`tree` + caller `session`) by case-insensitive substring across key, title, description, tags, and content. Returns matching keys, line/charOffset (for `box_read`), and bounded snippets."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Substring to search for (case-insensitive)."
                },
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Restrict search to `tree` or `session`."
                },
                "kind": {
                    "type": "string",
                    "enum": ["stable", "temporary"],
                    "description": "Restrict search to one kind."
                },
                "prefix": {
                    "type": "string",
                    "description": "Restrict to keys starting with this prefix."
                },
                "tag": {
                    "type": "string",
                    "description": "Restrict to boxes containing this exact tag."
                },
                "include_expired": {
                    "type": "boolean",
                    "description": "Also search expired temporary boxes (default false)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Maximum hits to return (default 20, cap 50)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let query = required_str(&input, "query")?;
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let kind = optional_enum(
            &input,
            "kind",
            &oc_store::BOX_KINDS,
            "`kind` must be `stable` or `temporary`",
        )?;
        let prefix = optional_str(&input, "prefix");
        let tag = optional_str(&input, "tag");
        let include_expired = optional_bool(&input, "include_expired")?.unwrap_or(false);
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(20, |value| usize::try_from(value).unwrap_or(20))
            .clamp(1, 50);

        let session = load_session_info(&self.runtime, &ctx.session_id).await?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let hits = self
            .runtime
            .inner
            .config
            .store
            .search_boxes(BoxSearchQuery {
                root_id: root.clone(),
                scope,
                session_id: Some(ctx.session_id.clone()),
                kind,
                prefix,
                tag,
                include_expired,
                query: query.clone(),
                limit,
            })
            .await
            .map_err(|error| store_error(&error))?;

        let mut summaries = Vec::new();
        let mut out = String::new();
        for hit in &hits {
            let key = hit.get("key").and_then(Value::as_str).unwrap_or_default();
            let effect = self
                .runtime
                .stored_session_effect(&session, "box:read", key);
            if effect == ConfigEffect::Deny {
                continue;
            }
            let summary = box_summary(hit);
            summaries.push(summary);
            let kind = hit.get("kind").and_then(Value::as_str).unwrap_or("stable");
            let box_scope = hit.get("scope").and_then(Value::as_str).unwrap_or("tree");
            let badge = if box_scope == "session" {
                format!("[{kind}, session]")
            } else {
                format!("[{kind}]")
            };
            let count = hit.get("matchCount").and_then(Value::as_u64).unwrap_or(1);
            let match_in = hit
                .get("matchIn")
                .and_then(Value::as_str)
                .unwrap_or("content");
            if effect == ConfigEffect::Ask {
                let _ = writeln!(
                    out,
                    "- {badge} {key} (match in {match_in}; snippet redacted pending box:read permission — call box_read)"
                );
                continue;
            }
            let location = match (
                hit.get("line").and_then(Value::as_u64),
                hit.get("charOffset").and_then(Value::as_u64),
            ) {
                (Some(line), Some(offset)) => {
                    format!(", line {line}, offset {offset}")
                }
                _ => String::new(),
            };
            let snippet = hit
                .get("snippet")
                .and_then(Value::as_str)
                .map(|text| format!("\n  {text}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- {badge} {key} ({count} match(es) in {match_in}{location}){snippet}"
            );
        }
        let header = format!("root {root}: {} match(es) for {query:?}\n", summaries.len());
        let full = if out.is_empty() {
            format!("{header}(no matching readable boxes)")
        } else {
            format!("{header}{out}")
        };
        Ok(ToolResult::text(full).with_metadata(json!({ "boxes": summaries })))
    }
}

struct BoxMetaTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxMetaTool {
    fn name(&self) -> &'static str {
        "box_meta"
    }

    fn description(&self) -> &'static str {
        "Update a context box's metadata or lifecycle without rewriting content: switch `kind` (`stable` <-> `temporary`), promote or demote `scope` (`session` <-> `tree`), rename (`new_key`), edit `title`/`description`/`tags`, or `revive` an expired temporary box."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Current box key."
                },
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Current scope when disambiguating; defaults to caller `session` then `tree`."
                },
                "new_scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Move the box between private `session` and shared `tree`."
                },
                "new_key": {
                    "type": "string",
                    "description": "Rename the box key."
                },
                "kind": {
                    "type": "string",
                    "enum": ["stable", "temporary"],
                    "description": "Change lifetime (`stable` or `temporary`)."
                },
                "title": {
                    "type": "string",
                    "description": "New short display title."
                },
                "description": {
                    "type": "string",
                    "description": "New one-line index description."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Replace the box tags list."
                },
                "revive": {
                    "type": "boolean",
                    "description": "Clear `timeExpired` on an expired temporary box."
                },
                "expected_revision": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optimistic lock against current revision."
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let key = required_str(&input, "key")?;
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let new_scope = optional_enum(
            &input,
            "new_scope",
            &BOX_SCOPES,
            "`new_scope` must be `tree` or `session`",
        )?;
        let new_key = optional_str(&input, "new_key");
        if let Some(target_key) = new_key.as_deref() {
            let session = load_session_info(&self.runtime, &ctx.session_id).await?;
            if self
                .runtime
                .stored_session_effect(&session, "box:write", target_key)
                == ConfigEffect::Deny
            {
                return Err(ToolError::new(format!(
                    "box:write permission denied for target key `{target_key}`"
                )));
            }
        }
        let kind = optional_enum(
            &input,
            "kind",
            &oc_store::BOX_KINDS,
            "`kind` must be `stable` or `temporary`",
        )?;
        let tags = optional_string_array(&input, "tags")?;
        let revive = optional_bool(&input, "revive")?.unwrap_or(false);
        let expected_revision = optional_i64(&input, "expected_revision")?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let updated = self
            .runtime
            .inner
            .config
            .store
            .update_box_meta(BoxMetaPatch {
                root_id: root,
                owner_session: ctx.session_id.clone(),
                scope,
                key,
                new_key,
                new_scope,
                kind,
                title: optional_str(&input, "title"),
                description: optional_str(&input, "description"),
                tags,
                revive,
                expected_revision,
            })
            .await
            .map_err(|error| match error {
                StoreError::Invalid(message) | StoreError::NotFound(message) => {
                    ToolError::new(message)
                }
                other => store_error(&other),
            })?;
        let next_key = updated
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let next_kind = updated
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("stable");
        let next_scope = updated
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("tree");
        let revision = updated.get("revision").and_then(Value::as_i64).unwrap_or(1);
        Ok(ToolResult::text(format!(
            "Box `{next_key}` updated [{next_kind}, {next_scope}], revision {revision}. The prompt index updates on the next turn."
        ))
        .with_metadata(json!({ "box": box_summary(&updated) })))
    }
}

struct BoxDeleteTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for BoxDeleteTool {
    fn name(&self) -> &'static str {
        "box_delete"
    }

    fn description(&self) -> &'static str {
        "Soft-delete a context box. Checks caller `session` scope before `tree` unless `scope` is specified. The key becomes reusable; the deleted row stays for audit and is hidden from box_list, box_read and the prompt index."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Box key to delete."
                },
                "scope": {
                    "type": "string",
                    "enum": ["tree", "session"],
                    "description": "Optional explicit scope (`tree` or `session`)."
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let key = required_str(&input, "key")?;
        let scope = optional_enum(
            &input,
            "scope",
            &BOX_SCOPES,
            "`scope` must be `tree` or `session`",
        )?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let deleted = self
            .runtime
            .inner
            .config
            .store
            .delete_box_scoped(&root, scope.as_deref(), Some(&ctx.session_id), &key)
            .await
            .map_err(|error| store_error(&error))?;
        if !deleted {
            return Err(ToolError::new(format!(
                "no live box `{key}` in root {root}"
            )));
        }
        Ok(ToolResult::text(format!(
            "Box `{key}` deleted. The prompt index updates on the next turn."
        )))
    }
}

struct CompactContextTool {
    runtime: Runtime,
}

#[async_trait::async_trait]
impl Tool for CompactContextTool {
    fn name(&self) -> &'static str {
        "compact_context"
    }

    fn description(&self) -> &'static str {
        "Request a context compaction at the next boundary. The summary replaces older history; stable boxes survive and temporary ones expire. Never compacts inside the current tool call."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "Optional reason recorded with the request."
                }
            }
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        // Coalescing: one pending `compaction` row per session. A second
        // request returns the row already queued instead of stacking another,
        // so `/compact` + this tool + an observer repair never run twice.
        let pending = self
            .runtime
            .inner
            .config
            .store
            .list_inbox(&ctx.session_id)
            .await
            .map_err(|error| store_error(&error))?
            .into_iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("compaction"));
        if let Some(item) = pending {
            let inbox_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            return Ok(ToolResult::text(format!(
                "Compaction already queued as `{inbox_id}`; it runs at the next turn boundary, \
                 after this turn ends. Coalesced with the pending request."
            ))
            .with_metadata(json!({ "inboxID": inbox_id, "coalesced": true })));
        }
        let mut payload = Map::new();
        payload.insert("time".to_owned(), json!({ "created": now_millis() }));
        payload.insert("status".to_owned(), Value::String("pending".to_owned()));
        payload.insert("reason".to_owned(), Value::String("manual".to_owned()));
        if let Some(note) = optional_str(&input, "note") {
            payload.insert("note".to_owned(), Value::String(note));
        }
        let item = self
            .runtime
            .enqueue_and_emit(NewInbox {
                id: MessageId::new().into_inner(),
                session_id: ctx.session_id.clone(),
                kind: "compaction".to_owned(),
                payload: Value::Object(payload),
                delivery: Delivery::Queue,
            })
            .await
            .map_err(|error| ToolError::new(format!("could not schedule compaction: {error}")))?;
        let inbox_id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.runtime
            .notify_local(&ctx.session_id)
            .await
            .map_err(|error| {
                ToolError::new(format!(
                    "compaction was enqueued as {inbox_id} but the actor could not be woken: {error}"
                ))
            })?;
        Ok(ToolResult::text(format!(
            "Compaction queued as `{inbox_id}`; it runs at the next turn boundary, after this turn \
             ends, so no summary exists yet. Stable boxes survive and temporary boxes expire."
        ))
        .with_metadata(json!({ "inboxID": inbox_id })))
    }
}

fn store_error(error: &StoreError) -> ToolError {
    ToolError::new(error.to_string())
}

fn required_str(input: &Value, key: &str) -> Result<String, ToolError> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| ToolError::new(format!("`{key}` is required and must be non-empty")))
}

fn optional_str(input: &Value, key: &str) -> Option<String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_bool(input: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::new(format!("`{key}` must be a boolean"))),
    }
}

fn optional_i64(input: &Value, key: &str) -> Result<Option<i64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| ToolError::new(format!("`{key}` must be an integer"))),
        Some(_) => Err(ToolError::new(format!("`{key}` must be an integer"))),
    }
}

fn optional_enum(
    input: &Value,
    key: &str,
    allowed: &[&str],
    err: &str,
) -> Result<Option<String>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if allowed.contains(&value.as_str()) => Ok(Some(value.clone())),
        Some(_) => Err(ToolError::new(err.to_owned())),
    }
}

fn optional_string_array(input: &Value, key: &str) -> Result<Option<Vec<String>>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| ToolError::new(format!("`{key}` must be strings")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(ToolError::new(format!("`{key}` must be an array"))),
    }
}

async fn load_session_info(
    runtime: &Runtime,
    session_id: &str,
) -> Result<oc_types::SessionInfo, ToolError> {
    let info = runtime
        .inner
        .config
        .store
        .get_session(session_id)
        .await
        .map_err(|error| store_error(&error))?
        .ok_or_else(|| ToolError::new(format!("session `{session_id}` no longer exists")))?;
    Runtime::parse_session(&info).map_err(|error| ToolError::new(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_renders_key_kind_size_and_description_only() {
        let boxes = vec![
            json!({
                "key": "decisions",
                "kind": "stable",
                "size": 12,
                "content": "SECRET BODY",
                "description": "architecture\ndecisions"
            }),
            json!({
                "key": "scratch",
                "kind": "temporary",
                "size": 80,
                "content": "more secret"
            }),
        ];
        let text = render_box_index("ses_root", &boxes, 100);
        assert!(text.contains("<boxes root=\"ses_root\">"), "{text}");
        assert!(
            text.contains("- [stable] decisions (12 chars) — architecture decisions"),
            "{text}"
        );
        assert!(text.contains("- [temporary] scratch (80 chars)"), "{text}");
        assert!(!text.contains("SECRET BODY"), "content stays out: {text}");
        assert!(!text.contains("more secret"), "{text}");
    }

    #[test]
    fn expired_and_overflowing_boxes_are_omitted_from_the_index() {
        let boxes = vec![json!({
            "key": "gone",
            "kind": "temporary",
            "size": 1,
            "timeExpired": 5,
        })];
        assert!(render_box_index("ses_root", &boxes, 100).is_empty());

        let many: Vec<Value> = (0..3)
            .map(|index| json!({"key": format!("k{index}"), "kind": "stable", "size": 1}))
            .collect();
        let text = render_box_index("ses_root", &many, 2);
        assert!(text.contains("k0") && text.contains("k1"), "{text}");
        assert!(!text.contains("k2 "), "{text}");
        assert!(text.contains("1 more boxes"), "{text}");
    }
}
