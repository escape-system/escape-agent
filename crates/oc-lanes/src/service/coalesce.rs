//! Coalescing key construction and lookup of the active ticket a new enqueue
//! may merge onto.

use crate::types::Ticket;

use super::State;

/// Finds the active ticket that `key` coalesces onto, registering `session` as
/// a subscriber. `None` means the caller must create a new ticket.
pub(super) fn coalesced_target(
    state: &mut State,
    key: Option<&CoalesceKey>,
    owner_session: Option<&str>,
) -> Option<Ticket> {
    let key = key?;
    let id = state
        .tickets
        .iter()
        .find(|(_, entry)| {
            entry.coalesce.as_ref() == Some(key) && !entry.ticket.state.is_terminal()
        })
        .map(|(id, _)| id.clone())?;
    let entry = state.tickets.get_mut(&id)?;
    if let Some(session) = owner_session
        && !entry
            .ticket
            .subscribers
            .iter()
            .any(|known| known == session)
    {
        entry.ticket.subscribers.push(session.to_owned());
    }
    Some(entry.ticket.clone())
}

/// Builds the coalescing key. `None` when the caller supplied no usable
/// `tree_rev`, which always creates a distinct ticket.
pub(super) fn coalesce_key(
    lane: &str,
    command: &str,
    cwd: &std::path::Path,
    tree_rev: Option<&str>,
) -> Option<CoalesceKey> {
    let tree_rev = tree_rev.map(str::trim).filter(|rev| !rev.is_empty())?;
    Some(CoalesceKey {
        lane: lane.to_owned(),
        command: normalize_command(command),
        cwd: cwd.to_path_buf(),
        tree_rev: tree_rev.to_owned(),
    })
}

/// Coalescing key: `(lane, normalized command, cwd, tree_rev)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CoalesceKey {
    lane: String,
    command: String,
    cwd: std::path::PathBuf,
    tree_rev: String,
}

/// Normalizes a command for coalescing only.
///
/// Trims, collapses runs of spaces/tabs into one space and strips a trailing
/// `;`. It never reorders flags, rewrites arguments or merges different
/// commands: two commands coalesce only when their tokens are identical after
/// this normalization.
fn normalize_command(command: &str) -> String {
    let mut normalized = String::with_capacity(command.len());
    for character in command.chars() {
        if character == ' ' || character == '\t' {
            if !normalized.is_empty() && !normalized.ends_with(' ') {
                normalized.push(' ');
            }
        } else {
            normalized.push(character);
        }
    }
    let trimmed = normalized.trim();
    let without_semicolon = trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end();
    without_semicolon.to_owned()
}
