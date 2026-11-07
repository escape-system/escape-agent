//! Stall Guard: detects degenerate tool-call loops and repeating cycles (Hermes `tool_guardrails` port).
//!
//! When an LLM gets trapped repeating the exact same tool call (e.g. calling `read`
//! on the same path 3 times, or ping-ponging in an A->B->A->B loop), `StallGuard`
//! detects the stagnation and generates a synthetic system warning to break the loop.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

use serde_json::{Map, Value};

const IDENTICAL_CALL_THRESHOLD: usize = 3;
const MAX_CYCLE_PERIOD: usize = 4;
const MAX_HISTORY: usize = 32;

/// Signature representing a tool call for deduplication / cycle detection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CallSignature {
    name: String,
    input_hash: u64,
}

impl CallSignature {
    fn new(name: &str, input: &Map<String, Value>) -> Self {
        let mut hasher = DefaultHasher::new();
        // Hash canonical JSON representation of input
        if let Ok(serialized) = serde_json::to_string(input) {
            serialized.hash(&mut hasher);
        }
        Self {
            name: name.to_owned(),
            input_hash: hasher.finish(),
        }
    }
}

/// Tracks tool calls during a turn to detect degenerate repeating patterns.
#[derive(Debug, Default)]
pub(super) struct StallGuard {
    history: VecDeque<CallSignature>,
    consecutive_repeats: usize,
    stalls_flagged: usize,
}

impl StallGuard {
    /// Records a new tool call and returns a synthetic warning message if a stall is detected.
    pub(super) fn record_and_check(
        &mut self,
        name: &str,
        input: &Map<String, Value>,
    ) -> Option<String> {
        // Exempt poller tools if any
        if name == "process_manage" || name.ends_with("_poll") {
            return None;
        }

        let sig = CallSignature::new(name, input);

        // Check 1: Consecutive identical tool call
        if let Some(last) = self.history.back() {
            if last == &sig {
                self.consecutive_repeats += 1;
            } else {
                self.consecutive_repeats = 1;
            }
        } else {
            self.consecutive_repeats = 1;
        }

        self.history.push_back(sig);
        if self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }

        if self.consecutive_repeats >= IDENTICAL_CALL_THRESHOLD {
            self.stalls_flagged += 1;
            return Some(format!(
                "[System: Stall Guard Warning: You have invoked tool `{name}` with identical arguments \
                 {} times consecutively without making forward progress. Cease repeating this exact call. \
                 Try an alternative strategy, check intermediate outputs, or explain the concrete blocker.]",
                self.consecutive_repeats
            ));
        }

        // Check 2: Cycle detection (e.g. A, B, A, B, A, B)
        if let Some((period, laps)) = self.detect_cycle()
            && laps >= IDENTICAL_CALL_THRESHOLD
        {
            self.stalls_flagged += 1;
            return Some(format!(
                "[System: Stall Guard Warning: You are caught in a repeating {period}-tool cycle \
                 ({laps} repetitions) without forward progress. Break this loop immediately: \
                 try a different approach or report the blocker.]"
            ));
        }

        None
    }

    /// Detects if the tail of the call history forms a periodic repeating cycle.
    fn detect_cycle(&self) -> Option<(usize, usize)> {
        let n = self.history.len();
        for period in 2..=MAX_CYCLE_PERIOD {
            if n < period * IDENTICAL_CALL_THRESHOLD {
                continue;
            }
            let mut laps = 0;
            let mut valid_cycle = true;

            for lap in 0..IDENTICAL_CALL_THRESHOLD {
                for offset in 0..period {
                    let idx_current = n - 1 - (lap * period + offset);
                    let idx_base = n - 1 - offset;
                    if self.history[idx_current] != self.history[idx_base] {
                        valid_cycle = false;
                        break;
                    }
                }
                if !valid_cycle {
                    break;
                }
                laps += 1;
            }

            if valid_cycle && laps >= IDENTICAL_CALL_THRESHOLD {
                return Some((period, laps));
            }
        }
        None
    }

    #[cfg(test)]
    #[must_use]
    pub(super) fn stalls_flagged(&self) -> usize {
        self.stalls_flagged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_consecutive_identical_calls() {
        let mut guard = StallGuard::default();
        let mut map = Map::new();
        map.insert("path".to_owned(), json!("src/main.rs"));

        assert!(guard.record_and_check("read", &map).is_none());
        assert!(guard.record_and_check("read", &map).is_none());
        let warning = guard.record_and_check("read", &map);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("Stall Guard Warning"));
        assert_eq!(guard.stalls_flagged(), 1);
    }

    #[test]
    fn varied_calls_do_not_trigger_consecutive_warning() {
        let mut guard = StallGuard::default();
        let mut map1 = Map::new();
        map1.insert("path".to_owned(), json!("src/main.rs"));

        let mut map2 = Map::new();
        map2.insert("path".to_owned(), json!("src/lib.rs"));

        assert!(guard.record_and_check("read", &map1).is_none());
        assert!(guard.record_and_check("read", &map2).is_none());
        assert!(guard.record_and_check("read", &map1).is_none());
        assert_eq!(guard.stalls_flagged(), 0);
    }

    #[test]
    fn detects_repeating_two_tool_cycles() {
        let mut guard = StallGuard::default();
        let mut map_a = Map::new();
        map_a.insert("path".to_owned(), json!("file_a.txt"));

        let mut map_b = Map::new();
        map_b.insert("path".to_owned(), json!("file_b.txt"));

        // Lap 1: A, B
        assert!(guard.record_and_check("read", &map_a).is_none());
        assert!(guard.record_and_check("grep", &map_b).is_none());
        // Lap 2: A, B
        assert!(guard.record_and_check("read", &map_a).is_none());
        assert!(guard.record_and_check("grep", &map_b).is_none());
        // Lap 3: A, B -> Stall
        assert!(guard.record_and_check("read", &map_a).is_none());
        let warning = guard.record_and_check("grep", &map_b);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("repeating 2-tool cycle"));
        assert_eq!(guard.stalls_flagged(), 1);
    }
}
