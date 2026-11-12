//! Scope vocabulary, validation and the rendered block's scope label.

/// Every scope the memory contract accepts.
pub(super) const SCOPES: [&str; 4] = ["global", "project", "agent", "session"];

use crate::error::MemoryError;

/// Rejects scopes that would read across projects by omission.
pub(crate) fn validate_scope(scope: &str, scope_id: Option<&str>) -> Result<(), MemoryError> {
    if !SCOPES.contains(&scope) {
        return Err(MemoryError::invalid(format!(
            "unknown memory scope {scope:?}; expected global, project, agent or session"
        )));
    }
    if scope == "global" {
        return if scope_id.is_none() {
            Ok(())
        } else {
            Err(MemoryError::invalid("global memories have no scope id"))
        };
    }
    match scope_id {
        Some(id) if !id.trim().is_empty() => Ok(()),
        _ => Err(MemoryError::invalid(format!(
            "scope {scope:?} needs a scope id"
        ))),
    }
}

/// Label for the rendered block: the most-shared named scope of the request.
pub(super) fn block_scope(scopes: &[(String, Option<String>)]) -> &str {
    scopes
        .iter()
        .find(|(scope, _)| scope == "project")
        .or_else(|| scopes.first())
        .map_or("project", |(scope, _)| scope.as_str())
}
