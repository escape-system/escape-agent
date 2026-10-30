//! The expectation registry: one terse [`FlightStrip`] per tool call.
//!
//! Only strips are kept. Tool arguments, outputs and assistant text never
//! enter this module. Settled strips live long enough for the observer and the
//! UI to reference them, then are pruned.
//!
//! Growth caps (per root):
//! - settled strips are dropped [`SETTLED_TTL_MS`] after settling;
//! - at most [`MAX_SETTLED_PER_ROOT`] settled strips are retained (oldest
//!   dropped first);
//! - running strips are bounded by actual in-flight tool calls.
//!
//! Both caps are documented in the crate docs and enforced on every mutation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::clock::Clock;

/// Maximum characters retained from an `outcome` one-liner.
pub const MAX_OUTCOME_CHARS: usize = 60;
/// Maximum characters retained from `why`.
pub const MAX_WHY_CHARS: usize = 160;
/// Settled strips are pruned after 24 hours.
pub const SETTLED_TTL_MS: i64 = 24 * 60 * 60 * 1_000;
/// Settled strips retained per root before the oldest are dropped.
pub const MAX_SETTLED_PER_ROOT: usize = 10_000;
/// Default active window for [`ExpectationRegistry::snapshot`].
pub const SNAPSHOT_ACTIVE: usize = 24;

/// Lifecycle state of a flight strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StripState {
    /// The tool call is still executing.
    #[default]
    Running,
    /// Completed successfully.
    Ok,
    /// Completed with an error.
    Error,
    /// Cancelled by the host, the user or a repair.
    Cancelled,
    /// Exceeded its intervention deadline and was stopped.
    TimedOut,
    /// Re-run by an observer repair.
    Repaired,
}

impl StripState {
    /// Whether the strip has finished.
    #[must_use]
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Running)
    }

    /// Whether this settlement is a failure worth waking the observer for.
    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Cancelled | Self::TimedOut)
    }

    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Repaired => "repaired",
        }
    }
}

/// One tool call as the observer sees it.
///
/// `why` is the only free text. `retryable` is host metadata (idempotent tool
/// or explicit `allow_retry`) and is never rendered into the model input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlightStrip {
    /// Observation id (`obs_…`).
    pub id: String,
    /// Root session that owns the call.
    pub root_id: String,
    /// Session that issued the call.
    pub session_id: String,
    /// Agent id.
    pub agent: String,
    /// Provider tool-call id.
    pub tool_call_id: String,
    /// Tool name.
    pub tool: String,
    /// One sentence: action + expected outcome.
    pub why: String,
    /// Agent estimate, seconds.
    pub expect_s: u32,
    /// Observer intervention deadline, seconds.
    pub intervene_s: u32,
    /// Open time, epoch ms.
    pub started_ms: i64,
    /// Settle time, epoch ms.
    pub settled_ms: Option<i64>,
    /// Lifecycle state.
    pub state: StripState,
    /// One-line result, capped at [`MAX_OUTCOME_CHARS`].
    pub outcome: Option<String>,
    /// Parent tool call (subagent nesting).
    pub parent_call: Option<String>,
    /// Session depth.
    pub depth: u32,
    /// Host metadata: may this tool call be retried by a repair?
    pub retryable: bool,
    /// Host metadata: the call this strip retries.
    pub retry_of: Option<String>,
}

impl Default for FlightStrip {
    fn default() -> Self {
        Self {
            id: String::new(),
            root_id: String::new(),
            session_id: String::new(),
            agent: String::new(),
            tool_call_id: String::new(),
            tool: String::new(),
            why: String::new(),
            expect_s: 0,
            intervene_s: 0,
            started_ms: 0,
            settled_ms: None,
            state: StripState::Running,
            outcome: None,
            parent_call: None,
            depth: 0,
            retryable: false,
            retry_of: None,
        }
    }
}

impl FlightStrip {
    /// Deadline at which the observer may intervene.
    #[must_use]
    pub fn deadline_ms(&self) -> i64 {
        self.started_ms
            .saturating_add(i64::from(self.intervene_s).saturating_mul(1_000))
    }

    /// True when the call is still running past its intervention deadline.
    #[must_use]
    pub fn is_overdue(&self, now_ms: i64) -> bool {
        self.state == StripState::Running && self.deadline_ms() <= now_ms
    }

    /// Elapsed seconds, clamped at zero.
    #[must_use]
    pub fn elapsed_s(&self, now_ms: i64) -> i64 {
        let end = self.settled_ms.unwrap_or(now_ms);
        (end - self.started_ms).max(0) / 1_000
    }
}

/// Retention knobs for the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryCaps {
    /// Settled strips retained per root.
    pub max_settled_per_root: usize,
    /// Settled strip time-to-live.
    pub settled_ttl_ms: i64,
}

impl Default for RegistryCaps {
    fn default() -> Self {
        Self {
            max_settled_per_root: MAX_SETTLED_PER_ROOT,
            settled_ttl_ms: SETTLED_TTL_MS,
        }
    }
}

/// A point-in-time view of one root's strips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrySnapshot {
    /// Running strips, most urgent first (overdue, then earliest deadline).
    pub active: Vec<FlightStrip>,
    /// Settled strips, newest first.
    pub recent: Vec<FlightStrip>,
}

#[derive(Debug, Default)]
struct RootIndex {
    /// Running tool-call ids in open order.
    active: Vec<String>,
    /// Settled tool-call ids, newest first.
    settled: VecDeque<String>,
}

impl RootIndex {
    fn remove_active(&mut self, tool_call_id: &str) -> bool {
        if let Some(pos) = self.active.iter().position(|id| id == tool_call_id) {
            self.active.remove(pos);
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Default)]
struct RegistryData {
    strips: HashMap<String, FlightStrip>,
    roots: HashMap<String, RootIndex>,
}

/// Per-root expectation registry.
///
/// The registry owns a [`Clock`] so settlement timestamps and TTL pruning are
/// deterministic in tests.
pub struct ExpectationRegistry {
    clock: Arc<dyn Clock>,
    caps: RegistryCaps,
    data: Mutex<RegistryData>,
}

impl std::fmt::Debug for ExpectationRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExpectationRegistry")
            .field("caps", &self.caps)
            .field("strips", &self.len())
            .finish_non_exhaustive()
    }
}

impl ExpectationRegistry {
    /// Create a registry with default retention caps.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_caps(clock, RegistryCaps::default())
    }

    /// Create a registry with explicit retention caps.
    #[must_use]
    pub fn with_caps(clock: Arc<dyn Clock>, caps: RegistryCaps) -> Self {
        Self {
            clock,
            caps,
            data: Mutex::new(RegistryData::default()),
        }
    }

    /// Retention caps.
    #[must_use]
    pub fn caps(&self) -> RegistryCaps {
        self.caps
    }

    /// Register a new strip. Running strips join the root's active index;
    /// already-settled strips join the recent index.
    pub fn open(&self, mut strip: FlightStrip) {
        strip.why = truncate_chars(&strip.why, MAX_WHY_CHARS);
        strip.outcome = strip
            .outcome
            .as_deref()
            .map(|outcome| truncate_chars(outcome, MAX_OUTCOME_CHARS));
        if strip.id.is_empty() {
            strip.id = format!("obs_{}", strip.tool_call_id);
        }

        let tool_call_id = strip.tool_call_id.clone();
        let root_id = strip.root_id.clone();
        let state = strip.state;
        let mut data = self.data.lock();

        if let Some(previous) = data.strips.remove(&tool_call_id)
            && let Some(index) = data.roots.get_mut(&previous.root_id)
        {
            index.remove_active(&tool_call_id);
            index.settled.retain(|id| id != &tool_call_id);
        }

        data.strips.insert(tool_call_id.clone(), strip);
        let index = data.roots.entry(root_id.clone()).or_default();
        if state == StripState::Running {
            index.active.push(tool_call_id);
        } else {
            index.settled.push_front(tool_call_id);
        }
        prune_root(&mut data, &root_id, self.caps, self.clock.now_ms());
    }

    /// Settle a strip. Returns false when the tool call is unknown.
    pub fn settle(&self, tool_call_id: &str, state: StripState, outcome: Option<&str>) -> bool {
        let now = self.clock.now_ms();
        self.settle_at(tool_call_id, state, outcome, now)
    }

    /// Settle a strip at an explicit timestamp.
    pub fn settle_at(
        &self,
        tool_call_id: &str,
        state: StripState,
        outcome: Option<&str>,
        now_ms: i64,
    ) -> bool {
        let mut data = self.data.lock();
        let Some(strip) = data.strips.get_mut(tool_call_id) else {
            return false;
        };
        let root_id = strip.root_id.clone();
        let was_running = strip.state == StripState::Running;
        strip.state = state;
        strip.settled_ms = Some(now_ms);
        strip.outcome = outcome.map(|text| truncate_chars(text, MAX_OUTCOME_CHARS));

        if let Some(index) = data.roots.get_mut(&root_id) {
            index.remove_active(tool_call_id);
            if was_running {
                index.settled.push_front(tool_call_id.to_owned());
            }
        }
        prune_root(&mut data, &root_id, self.caps, now_ms);
        true
    }

    /// Cancel every running strip of a session (root cancelled or interrupted).
    /// Returns the number of strips settled.
    pub fn cancel_session(&self, session_id: &str) -> usize {
        let now = self.clock.now_ms();
        let mut data = self.data.lock();
        let ids: Vec<String> = data
            .strips
            .values()
            .filter(|strip| strip.session_id == session_id && strip.state == StripState::Running)
            .map(|strip| strip.tool_call_id.clone())
            .collect();
        let mut roots = HashSet::new();
        for id in &ids {
            if let Some(strip) = data.strips.get_mut(id) {
                strip.state = StripState::Cancelled;
                strip.settled_ms = Some(now);
                strip.outcome = Some("cancelled".to_owned());
                roots.insert(strip.root_id.clone());
            }
            if let Some(root_id) = data.strips.get(id).map(|strip| strip.root_id.clone())
                && let Some(index) = data.roots.get_mut(&root_id)
            {
                index.remove_active(id);
                index.settled.push_front(id.clone());
            }
        }
        for root_id in roots {
            prune_root(&mut data, &root_id, self.caps, now);
        }
        ids.len()
    }

    /// Running strips for a root, in open order.
    #[must_use]
    pub fn active(&self, root_id: &str) -> Vec<FlightStrip> {
        let data = self.data.lock();
        let Some(index) = data.roots.get(root_id) else {
            return Vec::new();
        };
        index
            .active
            .iter()
            .filter_map(|id| data.strips.get(id))
            .filter(|strip| strip.state == StripState::Running)
            .cloned()
            .collect()
    }

    /// Settled strips for a root, newest first.
    #[must_use]
    pub fn recent(&self, root_id: &str, limit: usize) -> Vec<FlightStrip> {
        let data = self.data.lock();
        let Some(index) = data.roots.get(root_id) else {
            return Vec::new();
        };
        index
            .settled
            .iter()
            .filter_map(|id| data.strips.get(id))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Earliest intervention deadline among running strips.
    #[must_use]
    pub fn earliest_deadline(&self, root_id: &str) -> Option<i64> {
        self.active(root_id)
            .into_iter()
            .map(|strip| strip.deadline_ms())
            .min()
    }

    /// Active-first snapshot. Overdue strips are never dropped; the active
    /// window is otherwise capped at `max_active`.
    #[must_use]
    pub fn snapshot(&self, root_id: &str, max_active: usize) -> RegistrySnapshot {
        let now = self.clock.now_ms();
        let mut active = self.active(root_id);
        active.sort_by_key(|strip| {
            let overdue = !strip.is_overdue(now);
            (overdue, strip.deadline_ms(), strip.started_ms)
        });
        if active.len() > max_active {
            let mut kept = Vec::with_capacity(max_active.max(1));
            let mut overflow = active.len() - max_active;
            for strip in active {
                if overflow > 0 && !strip.is_overdue(now) {
                    overflow -= 1;
                    continue;
                }
                kept.push(strip);
            }
            active = kept;
        }
        RegistrySnapshot {
            active,
            recent: self.recent(root_id, max_active.max(SNAPSHOT_ACTIVE)),
        }
    }

    /// Look up a strip by tool-call id.
    #[must_use]
    pub fn strip(&self, tool_call_id: &str) -> Option<FlightStrip> {
        self.data.lock().strips.get(tool_call_id).cloned()
    }

    /// Number of retained strips (running + settled).
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.lock().strips.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn prune_root(data: &mut RegistryData, root_id: &str, caps: RegistryCaps, now_ms: i64) {
    let Some(index) = data.roots.get_mut(root_id) else {
        return;
    };
    let mut evicted = Vec::new();
    while let Some(id) = index.settled.back() {
        let expired = data
            .strips
            .get(id)
            .and_then(|strip| strip.settled_ms)
            .is_some_and(|settled| settled.saturating_add(caps.settled_ttl_ms) <= now_ms);
        let over_cap = index.settled.len() > caps.max_settled_per_root;
        if !expired && !over_cap {
            break;
        }
        if let Some(id) = index.settled.pop_back() {
            evicted.push(id);
        }
    }
    for id in evicted {
        data.strips.remove(&id);
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        text.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[derive(Debug, Default)]
    struct FakeClock(AtomicI64);

    impl FakeClock {
        fn set(&self, value: i64) {
            self.0.store(value, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn strip(id: &str, root: &str, session: &str, started: i64, intervene: u32) -> FlightStrip {
        FlightStrip {
            id: format!("obs_{id}"),
            root_id: root.to_owned(),
            session_id: session.to_owned(),
            agent: "build".to_owned(),
            tool_call_id: id.to_owned(),
            tool: "shell".to_owned(),
            why: "run focused test".to_owned(),
            expect_s: 10,
            intervene_s: intervene,
            started_ms: started,
            retryable: true,
            ..FlightStrip::default()
        }
    }

    #[test]
    fn lifecycle_and_overdue() {
        let clock = Arc::new(FakeClock::default());
        clock.set(1_000);
        let registry = ExpectationRegistry::new(clock.clone());
        registry.open(strip("tc_1", "ses_root", "ses_root", 1_000, 10));

        assert_eq!(registry.active("ses_root").len(), 1);
        assert_eq!(registry.earliest_deadline("ses_root"), Some(11_000));

        clock.set(10_999);
        assert!(!registry.active("ses_root")[0].is_overdue(clock.now_ms()));
        clock.set(11_000);
        assert!(registry.active("ses_root")[0].is_overdue(clock.now_ms()));

        let long_outcome = "x".repeat(200);
        assert!(registry.settle("tc_1", StripState::Ok, Some(&long_outcome)));
        let settled = &registry.recent("ses_root", 1)[0];
        assert_eq!(settled.state, StripState::Ok);
        assert_eq!(settled.settled_ms, Some(11_000));
        let outcome = settled.outcome.as_deref().unwrap_or_default();
        assert_eq!(outcome.chars().count(), MAX_OUTCOME_CHARS);
        assert!(registry.active("ses_root").is_empty());
        assert!(!registry.settle("tc_missing", StripState::Ok, None));

        registry.open(strip("tc_2", "ses_root", "ses_child", 11_000, 10));
        assert_eq!(registry.cancel_session("ses_child"), 1);
        assert_eq!(registry.active("ses_root").len(), 0);
        assert_eq!(registry.recent("ses_root", 5)[0].tool_call_id, "tc_2");
    }

    #[test]
    fn settled_strips_are_pruned() {
        let clock = Arc::new(FakeClock::default());
        clock.set(0);
        let registry = ExpectationRegistry::with_caps(
            clock.clone(),
            RegistryCaps {
                max_settled_per_root: 2,
                settled_ttl_ms: SETTLED_TTL_MS,
            },
        );
        for index in 0..4 {
            let id = format!("tc_{index}");
            registry.open(strip(&id, "ses_root", "ses_root", 0, 10));
            registry.settle(&id, StripState::Ok, Some("ok"));
        }
        assert_eq!(registry.recent("ses_root", 10).len(), 2);

        clock.set(SETTLED_TTL_MS + 1);
        registry.open(strip("tc_new", "ses_root", "ses_root", clock.now_ms(), 10));
        assert_eq!(registry.recent("ses_root", 10).len(), 0);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn snapshot_caps_active_but_keeps_overdue() {
        let clock = Arc::new(FakeClock::default());
        clock.set(100_000);
        let registry = ExpectationRegistry::new(clock);
        for index in 0..30 {
            let started = if index < 2 { 0 } else { 99_000 };
            let id = format!("tc_{index}");
            registry.open(strip(&id, "ses_root", "ses_root", started, 10));
        }
        let snapshot = registry.snapshot("ses_root", SNAPSHOT_ACTIVE);
        assert_eq!(snapshot.active.len(), SNAPSHOT_ACTIVE);
        let overdue: Vec<_> = snapshot
            .active
            .iter()
            .filter(|strip| strip.is_overdue(100_000))
            .collect();
        assert_eq!(overdue.len(), 2);
        assert_eq!(
            snapshot.active[0].tool_call_id, "tc_0",
            "overdue strips sort first"
        );
    }
}
