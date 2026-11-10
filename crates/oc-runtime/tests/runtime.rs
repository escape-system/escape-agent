//! Offline end-to-end tests for the session actor and turn loop.
//!
//! Every test uses a temp SQLite database, a temp config tree and a scripted
//! model client; no network credentials or HTTP servers are involved.

#[path = "runtime/actor_death.rs"]
mod actor_death;
#[path = "runtime/actor_error_terminal.rs"]
mod actor_error_terminal;
#[path = "runtime/context_boxes.rs"]
mod context_boxes;
#[path = "runtime/instruction_scope.rs"]
mod instruction_scope;
#[path = "runtime/lane_subscriber.rs"]
mod lane_subscriber;
#[path = "runtime/memory.rs"]
mod memory;
#[path = "runtime/observer.rs"]
mod observer;
#[path = "runtime/observer_advice.rs"]
mod observer_advice;
#[path = "runtime/recovery.rs"]
mod recovery;
#[path = "runtime/stop.rs"]
mod stop;
#[path = "runtime/subagents.rs"]
mod subagents;
#[path = "runtime/support.rs"]
mod support;
#[path = "runtime/tools_goals.rs"]
mod tools_goals;
#[path = "runtime/turn_core.rs"]
mod turn_core;
#[path = "runtime/write_budget.rs"]
mod write_budget;
