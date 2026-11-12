//! Wake decisions: which tree/strip shapes count as "the observer must look",
//! plus the overdue backoff that stops one long call re-waking the model.

use crate::input::TreeRow;
use crate::registry::FlightStrip;

pub(super) fn is_user_stopped_row(row: &TreeRow) -> bool {
    row.state == "interrupted" && row.idle_reason.as_deref() == Some("user")
}

pub(super) fn is_strip_overdue(strip: &FlightStrip, tree: &[TreeRow], now: i64) -> bool {
    if !strip.is_overdue(now) {
        return false;
    }
    if strip.tool == "subagent"
        && tree.iter().any(|row| {
            row.parent.as_deref() == Some(strip.session_id.as_str()) && row.state == "busy"
        })
    {
        return false;
    }
    true
}
