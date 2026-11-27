//! Stall Guard: deterministic SHA-256 tool-call Loop Guard (Hermes + `OpenFang` pattern).
//!
//! When an LLM gets trapped repeating the exact same tool call (canonical JSON
//! arguments hashed with SHA-256) or repeating identical failing calls/cycles,
//! `StallGuard` detects the stagnation and injects a synthetic system warning
//! to break the loop (`docs/30-tos-safe-harness.md`).

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

const IDENTICAL_CALL_THRESHOLD: usize = 3;
const IDENTICAL_FAILURE_THRESHOLD: usize = 3;
const MAX_CYCLE_PERIOD: usize = 4;
const MAX_HISTORY: usize = 32;

/// Deterministic SHA-256 signature representing a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CallSignature {
    name: String,
    input_sha256: [u8; 32],
}

fn canonicalize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), canonicalize_value(v));
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_value).collect()),
        other => other.clone(),
    }
}

fn sha256_call(name: &str, input: &Map<String, Value>) -> [u8; 32] {
    let canonical = canonicalize_value(&Value::Object(input.clone()));
    let serialized = serde_json::to_string(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0_u8]);
    hasher.update(serialized.as_bytes());
    hasher.finalize().into()
}

impl CallSignature {
    fn new(name: &str, input: &Map<String, Value>) -> Self {
        Self {
            name: name.to_owned(),
            input_sha256: sha256_call(name, input),
        }
    }
}

/// Tracks tool calls and repeated identical failures during a turn using SHA-256 digests.
#[derive(Debug, Default)]
pub(super) struct StallGuard {
    history: VecDeque<CallSignature>,
    failure_history: VecDeque<([u8; 32], [u8; 32])>,
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

    /// Record a failed tool execution by `(call_sha256, error_sha256)`. Returns
    /// a loop-guard warning if the same normalized failure repeats in the sliding window.
    pub(super) fn record_failure(
        &mut self,
        name: &str,
        input: &Map<String, Value>,
        error_text: &str,
    ) -> Option<String> {
        let call_hash = sha256_call(name, input);
        let mut err_hasher = Sha256::new();
        err_hasher.update(error_text.trim().as_bytes());
        let err_hash: [u8; 32] = err_hasher.finalize().into();

        self.failure_history.push_back((call_hash, err_hash));
        if self.failure_history.len() > MAX_HISTORY {
            self.failure_history.pop_front();
        }
        let count = self
            .failure_history
            .iter()
            .filter(|(c, e)| *c == call_hash && *e == err_hash)
            .count();
        if count >= IDENTICAL_FAILURE_THRESHOLD {
            self.stalls_flagged += 1;
            return Some(format!(
                "[System: Loop Guard Warning: Tool `{name}` produced the identical failure {count} times \
                 in the recent window. Stop retrying the failing call; inspect the root cause or switch strategy.]"
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

    #[test]
    fn sha256_canonicalizes_key_order_and_flags_repeated_failures() {
        let mut guard = StallGuard::default();
        let a: Map<String, Value> = serde_json::from_value(json!({"a": 1, "b": 2})).unwrap();
        let b: Map<String, Value> = serde_json::from_value(json!({"b": 2, "a": 1})).unwrap();
        assert!(guard.record_and_check("edit", &a).is_none());
        assert!(guard.record_and_check("edit", &b).is_none());
        let w = guard.record_and_check("edit", &a);
        assert!(
            w.is_some(),
            "key-order differences must hash identically via canonical SHA-256"
        );

        let mut fail_guard = StallGuard::default();
        assert!(fail_guard.record_failure("shell", &a, "exit 1").is_none());
        assert!(fail_guard.record_failure("shell", &b, "exit 1").is_none());
        let fw = fail_guard.record_failure("shell", &a, "exit 1");
        assert!(fw.is_some_and(|m| m.contains("Loop Guard Warning")));
    }
}
