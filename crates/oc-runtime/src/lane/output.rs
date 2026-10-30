use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use oc_lanes::TicketState;
use oc_tools::util;

use super::{MAX_BYTES, MAX_LINES};

// ---------------------------------------------------------------------------
// Output capture
// ---------------------------------------------------------------------------

pub(super) struct Tail {
    pub(super) text: String,
    pub(super) truncated: bool,
}

/// Reads at most [`MAX_BYTES`] from the end of `path` and keeps at most
/// [`MAX_LINES`] trailing lines.
pub(super) fn read_tail(path: &Path) -> io::Result<Tail> {
    let length = fs::metadata(path)?.len();
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(length.saturating_sub(MAX_BYTES as u64)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let tail = String::from_utf8_lossy(&bytes).into_owned();
    let mut lines: Vec<&str> = tail.split('\n').collect();
    if tail.ends_with('\n') {
        lines.pop();
    }
    let total = util::count_lines(path)?;
    let truncated = length > MAX_BYTES as u64 || total > MAX_LINES;
    let text = if lines.len() > MAX_LINES {
        lines[lines.len() - MAX_LINES..].join("\n")
    } else {
        tail
    };
    Ok(Tail { text, truncated })
}

pub(super) fn tail_body(tail: Tail) -> String {
    if tail.text.is_empty() {
        "(no output)".to_owned()
    } else if tail.truncated {
        format!(
            "{}\n\n[truncated to the last {} lines / {} KiB]",
            tail.text,
            MAX_LINES,
            MAX_BYTES / 1024
        )
    } else {
        tail.text
    }
}

pub(super) fn status_name(state: &TicketState) -> &'static str {
    match state {
        TicketState::Queued => "queued",
        TicketState::Running => "running",
        TicketState::Succeeded { .. } => "succeeded",
        TicketState::Failed { .. } => "failed",
        TicketState::Cancelled => "cancelled",
    }
}

pub(super) fn status_exit(state: &TicketState) -> Option<i32> {
    match state {
        TicketState::Succeeded { exit } | TicketState::Failed { exit, .. } => Some(*exit),
        TicketState::Queued | TicketState::Running | TicketState::Cancelled => None,
    }
}
