//! Wake decisions: which tree/strip shapes count as "the observer must look",
//! plus the overdue backoff that stops one long call re-waking the model.

use crate::clock::EngineNow;
use crate::input::TreeRow;
use crate::registry::FlightStrip;

pub(super) fn is_user_stopped_row(row: &TreeRow) -> bool {
    row.state == "interrupted" && row.idle_reason.as_deref() == Some("user")
}

/// True when `strip` is past its intervention deadline.
///
/// Only `now.mono_ms` is consulted: the deadline is a monotonic duration, so a
/// wall-clock jump cannot make a fresh call look overdue (forward jump) or an
/// overdue call look fresh (backward jump).
pub(super) fn is_strip_overdue(strip: &FlightStrip, tree: &[TreeRow], now: EngineNow) -> bool {
    if !strip.is_overdue(now.mono_ms) {
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
