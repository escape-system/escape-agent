//! Observation registry, LLM observer loop, repairs and goal supervision.
//!
//! The observer sees only terse **flight strips**: `why`, clocks, statuses and
//! tree ids/statuses. It never receives tool arguments, tool output, files or
//! message text. It acts through host-supplied commands, so a misbehaving
//! model cannot re-enter the agent loop.
//!
//! # Host wiring
//!
//! - The runtime calls [`ObserverEngine::observe_start`] when a tool call
//!   begins and [`ObserverEngine::observe_settle`] when it finishes; execution
//!   failures/interrupts and step events go through
//!   [`ObserverEngine::note_host_event`].
//! - The actor calls [`ObserverEngine::tick`] on `observer.tick_ms`, and the
//!   UI route calls [`ObserverEngine::check_now`]. A run is one model call,
//!   coalesced to `min_interval_ms` / `max_calls_per_min`.
//! - [`ObserverModel`] is implemented over `oc-llm`; [`RepairExecutor`] sends
//!   commands to the runtime; [`ObserverNotifier`] publishes `observer.*`
//!   events, writes the repair log and injects synthetic messages.
//! - A DB-backed `GoalStore` persists [`GoalSnapshot`] rows
//!   (`goal(id, root_id, objective, criteria_json, status, rounds, stalls,
//!   last_progress_ms, evidence_json, …)`) and supplies the snapshot on every
//!   tick, including the recent tool/message ids in `known_ids`.
//!
//! # Growth caps
//!
//! Settled strips are pruned 24 h after settlement and capped at 10 000 per
//! root (oldest first); running strips are bounded by in-flight tool calls.

pub mod clock;
pub mod config;
pub mod engine;
pub mod error;
pub mod goal;
pub mod input;
pub mod persistence;
pub mod registry;
pub mod verdict;

pub use clock::{Clock, SystemClock};
pub use config::{GoalConfig, ObserverConfig};
pub use engine::{
    BoxFuture, BudgetState, HostFlag, ObserverEngine, ObserverModel, ObserverNotice,
    ObserverNotifier, ObserverRun, RepairExecutor, RepairOutcome, SkipReason, TargetBudget,
};
pub use error::ObserverError;
pub use goal::{
    GoalPolicy, GoalSnapshot, GoalStatus, GoalTransition, GoalVerdict, GoalVerdictKind,
    RecentOutcome, apply_verdict, judge_prompt, parse_goal_verdict,
};
pub use input::{
    ActiveStrip, GoalInput, ObserverInput, RecentStrip, RootContext, RootInput, TreeRow, render,
};
pub use persistence::{GoalStore, RepairLog, RepairRecord};
pub use registry::{
    ExpectationRegistry, FlightStrip, MAX_OUTCOME_CHARS, MAX_SETTLED_PER_ROOT, MAX_WHY_CHARS,
    RegistryCaps, RegistrySnapshot, SETTLED_TTL_MS, SNAPSHOT_ACTIVE, StripState,
};
pub use verdict::{
    MAX_REASON_CHARS, MAX_REASON_WORDS, ObserverAction, ObserverVerdict, Repair, RepairKind,
    VerdictError, parse_verdict,
};
