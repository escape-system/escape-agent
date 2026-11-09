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
use oc_store::{BoxQuery, Delivery, NewBox, NewInbox, StoreError};
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

    /// Registry entries: `box_list`, `box_read`, `box_write`, `box_delete` and
    /// `compact_context`.
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
        let size = value.get("size").and_then(Value::as_u64).unwrap_or(0);
        let note = value
            .get("description")
            .or_else(|| value.get("title"))
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|note| !note.is_empty());
        match note {
            Some(note) => {
                let _ = writeln!(out, "- [{kind}] {key} ({size} chars) — {note}");
            }
            None => {
                let _ = writeln!(out, "- [{kind}] {key} ({size} chars)");
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
    const FIELDS: [&str; 11] = [
        "id",
        "key",
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

    /// Live boxes of the session's tree, ordered by key.
    ///
    /// Fetches one row past [`BOX_INDEX_MAX`] so [`render_box_index`] can prove
    /// that more boxes exist and emit its truncation notice instead of
    /// truncating silently.
    pub(crate) async fn box_index_rows(&self, root_id: &str) -> Result<Vec<Value>, ToolError> {
        self.inner
            .config
            .store
            .list_boxes(BoxQuery {
                root_id: root_id.to_owned(),
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
        "List the durable context boxes shared by this session tree: key, kind (stable/temporary), size and description. Content is not included; call box_read for it."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["stable", "temporary"],
                    "description": "Only boxes of this kind."
                },
                "include_expired": {
                    "type": "boolean",
                    "description": "Include temporary boxes expired by a compaction (default false)."
                }
            }
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let kind = match input.get("kind") {
            None | Some(Value::Null) => None,
            Some(Value::String(kind)) if oc_store::BOX_KINDS.contains(&kind.as_str()) => {
                Some(kind.clone())
            }
            Some(_) => {
                return Err(ToolError::new(
                    "`kind` must be `stable` or `temporary`".to_owned(),
                ));
            }
        };
        let include_expired = optional_bool(&input, "include_expired")?.unwrap_or(false);
        // Every box operation is scoped to the tree, but read visibility is
        // per key: a caller denied `box:read` on a key must not learn it from
        // the listing. Load the session once and evaluate each row's key.
        let info = self
            .runtime
            .inner
            .config
            .store
            .get_session(&ctx.session_id)
            .await
            .map_err(|error| store_error(&error))?
            .ok_or_else(|| {
                ToolError::new(format!("session `{}` no longer exists", ctx.session_id))
            })?;
        let session =
            Runtime::parse_session(&info).map_err(|error| ToolError::new(error.to_string()))?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let page = self
            .runtime
            .inner
            .config
            .store
            .list_boxes(BoxQuery {
                root_id: root.clone(),
                kind,
                include_expired,
                limit: BOX_INDEX_MAX,
            })
            .await
            .map_err(|error| store_error(&error))?;
        // A denied key is invisible, exactly as `box_read` refuses it. `ask`
        // is not a deny: listing stays non-interactive (the prompt would be
        // raised by the read itself), so only `Deny` rows drop out.
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
        let mut out = String::new();
        let summaries: Vec<Value> = visible.iter().map(|item| box_summary(item)).collect();
        let _ = writeln!(out, "root {root}: {} box(es)", visible.len());
        if page.has_more {
            out.push_str("(more boxes exist than are listed)\n");
        }
        for item in &visible {
            let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
            let kind = item.get("kind").and_then(Value::as_str).unwrap_or("stable");
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
                "- [{kind}] {key} ({size} chars, rev {revision}){expired}{note}"
            );
        }
        Ok(ToolResult::text(out).with_metadata(json!({
            "boxes": summaries,
            "hasMore": page.has_more,
        })))
    }
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
        "Read a durable context box by key. Content is returned in chunks of at most 20000 characters; use offset/limit for larger boxes. Expired temporary boxes are hidden unless include_expired is set."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Box key inside this session tree."
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
        let include_expired = optional_bool(&input, "include_expired")?.unwrap_or(false);
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
            .get_box(&root, &key, include_expired)
            .await
            .map_err(|error| store_error(&error))?
        else {
            let expired = self
                .runtime
                .inner
                .config
                .store
                .get_box(&root, &key, true)
                .await
                .map_err(|error| store_error(&error))?
                .is_some();
            return Err(ToolError::new(if expired {
                format!(
                    "box `{key}` was expired by a compaction; set include_expired to read it anyway"
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
        let start = offset.min(total);
        let chunk: String = content.chars().skip(start).take(limit).collect();
        let end = start + chunk.chars().count();
        let kind = box_value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("stable");
        let mut out =
            format!("box `{key}` [{kind}] {total} chars, showing {start}..{end}\n\n{chunk}");
        if end < total {
            let _ = write!(
                out,
                "\n\n[{} chars remain; call box_read with offset={end}]",
                total - end
            );
        }
        Ok(ToolResult::text(out).with_metadata(json!({
            "key": key,
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
        "Create or update a durable context box shared by this session tree. `stable` boxes survive compaction; `temporary` boxes expire at the next one. Use append to extend a box; a write revives an expired or deleted key."
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
                "kind": {
                    "type": "string",
                    "enum": ["stable", "temporary"],
                    "description": "Lifetime; defaults to `stable`."
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
                    "description": "Free-form tags."
                },
                "append": {
                    "type": "boolean",
                    "description": "Append content to the existing box instead of replacing it."
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
                "`content` must not be empty or whitespace-only; write real text or delete the box",
            ));
        }
        if content.chars().count() > MAX_BOX_WRITE_CHARS {
            return Err(ToolError::new(format!(
                "one box_write call accepts at most {MAX_BOX_WRITE_CHARS} characters"
            )));
        }
        let kind = match input.get("kind") {
            None | Some(Value::Null) => "stable".to_owned(),
            Some(Value::String(kind)) if oc_store::BOX_KINDS.contains(&kind.as_str()) => {
                kind.clone()
            }
            Some(_) => {
                return Err(ToolError::new(
                    "`kind` must be `stable` or `temporary`".to_owned(),
                ));
            }
        };
        let append = optional_bool(&input, "append")?.unwrap_or(false);
        let tags = match input.get("tags") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| ToolError::new("`tags` must be strings".to_owned()))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(ToolError::new("`tags` must be an array".to_owned())),
        };
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let written = self
            .runtime
            .inner
            .config
            .store
            .write_box(
                NewBox {
                    root_id: root,
                    owner_session: ctx.session_id.clone(),
                    key: key.clone(),
                    title: optional_str(&input, "title"),
                    description: optional_str(&input, "description"),
                    kind,
                    tags,
                    content: content.to_owned(),
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
        Ok(ToolResult::text(format!(
            "Box `{key}` written [{kind}], {size} chars, revision {revision}. The prompt index updates on the next turn."
        ))
        .with_metadata(json!({ "box": box_summary(&written) })))
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
        "Soft-delete a context box. The key becomes reusable; the deleted row stays for audit and is hidden from box_list, box_read and the prompt index."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Box key to delete."
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let key = required_str(&input, "key")?;
        let root = self.runtime.box_root(&ctx.session_id).await?;
        let deleted = self
            .runtime
            .inner
            .config
            .store
            .delete_box(&root, &key)
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
        Ok(ToolResult::text(
            "Compaction scheduled; it runs at the next boundary, after this turn. Stable boxes survive and temporary boxes expire.",
        )
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
