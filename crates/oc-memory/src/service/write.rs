//! Write and forget paths: the character cap, the secret detector, the
//! documented defaults and the soft-delete probe.

use oc_store::NewMemory;
use oc_types::MemoryId;
use serde_json::Value;

use crate::error::MemoryError;
use crate::secret::ensure_no_secret;

use super::input::normalized;
use super::scope::SCOPES;
use super::{DEFAULT_CONFIDENCE, DEFAULT_IMPORTANCE, MemoryService, NewMemoryInput, TEXT_CAP};

impl MemoryService {
    /// Writes a memory, folding an equivalent row (same scope and normalized
    /// text) instead of duplicating it. Returns the stored wire row.
    pub async fn write(&self, new: NewMemoryInput) -> Result<Value, MemoryError> {
        let text = new.text.trim().to_owned();
        if text.is_empty() {
            return Err(MemoryError::invalid("memory text must not be empty"));
        }
        let chars = text.chars().count();
        if chars > TEXT_CAP {
            return Err(MemoryError::TextTooLong {
                chars,
                cap: TEXT_CAP,
            });
        }
        ensure_no_secret(&text)?;

        let kind = normalized(new.kind.as_deref(), "fact");
        if kind.is_empty() {
            return Err(MemoryError::invalid("memory kind must not be empty"));
        }
        let scope = normalized(new.scope.as_deref(), "project");
        if !SCOPES.contains(&scope.as_str()) {
            return Err(MemoryError::invalid(format!(
                "unknown memory scope {scope:?}; expected global, project, agent or session"
            )));
        }
        let scope_id = if scope == "global" {
            None
        } else {
            let id = new.scope_id.as_deref().map(str::trim).unwrap_or_default();
            if id.is_empty() {
                return Err(MemoryError::invalid(format!(
                    "scope {scope:?} needs a scope id"
                )));
            }
            Some(id.to_owned())
        };

        let source = normalized(new.source.as_deref(), "agent");
        let memory = NewMemory {
            id: MemoryId::new().into_inner(),
            scope,
            scope_id,
            kind,
            text,
            tags: new.tags,
            importance: new.importance.unwrap_or(DEFAULT_IMPORTANCE),
            confidence: new.confidence.unwrap_or(DEFAULT_CONFIDENCE),
            source,
            source_session: new.source_session,
            source_message: new.source_message,
        };
        Ok(self.store.write_memory(memory).await?)
    }

    /// Soft-deletes a memory. Returns whether a live row was changed.
    pub async fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(MemoryError::invalid("memory id must not be empty"));
        }
        Ok(self.store.forget_memory(id).await?)
    }
}
