//! Durable saved-rule persistence against the `permission_saved` table.

use oc_store::{Store, StoreError};
use serde_json::Value;

use super::state::{Pending, Rule};

/// Project owning `session_id`, read from the `Session.Info.projectID` column.
async fn project_of(store: &Store, session_id: &str) -> Result<Option<String>, StoreError> {
    Ok(store.get_session(session_id).await?.and_then(|session| {
        session
            .get("projectID")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }))
}

/// Durable saved rules of the project owning `session_id`.
pub(super) async fn durable_rules(
    store: &Store,
    session_id: &str,
) -> Result<Vec<Rule>, StoreError> {
    let Some(project_id) = project_of(store, session_id).await? else {
        return Ok(Vec::new());
    };
    let saved = store.list_saved_permissions(Some(&project_id)).await?;
    Ok(saved
        .iter()
        .filter_map(|row| {
            Some(Rule {
                action: row.get("action")?.as_str()?.to_owned(),
                resources: vec![row.get("resource")?.as_str()?.to_owned()],
                durable: true,
            })
        })
        .collect())
}

/// Writes every resource of an `always` answer to the session's project.
/// Returns whether all writes committed; the caller mirrors the rule in memory
/// otherwise.
pub(super) async fn persist(store: &Store, entry: &Pending) -> bool {
    let project_id = match project_of(store, &entry.session_id).await {
        Ok(Some(project_id)) => project_id,
        Ok(None) => {
            tracing::warn!(
                session = %entry.session_id,
                "saved permission rule kept in memory: the session row is missing"
            );
            return false;
        }
        Err(error) => {
            tracing::warn!(
                session = %entry.session_id,
                error = %error,
                "saved permission rule kept in memory: the session lookup failed"
            );
            return false;
        }
    };
    let mut persisted = true;
    for resource in &entry.resources {
        if let Err(error) = store
            .save_permission_rule(&project_id, &entry.action, resource)
            .await
        {
            tracing::warn!(
                project = %project_id,
                action = %entry.action,
                resource = %resource,
                error = %error,
                "saved permission write failed"
            );
            persisted = false;
        }
    }
    persisted
}
