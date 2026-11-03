//! Offline end-to-end tests for the session actor and turn loop.
//!
//! Every test uses a temp SQLite database, a temp config tree and a scripted
//! model client; no network credentials or HTTP servers are involved.

#[path = "runtime/instruction_scope.rs"]
mod instruction_scope;
#[path = "runtime/memory.rs"]
mod memory;
#[path = "runtime/observer.rs"]
mod observer;
#[path = "runtime/recovery.rs"]
mod recovery;
#[path = "runtime/subagents.rs"]
mod subagents;
#[path = "runtime/support.rs"]
mod support;
#[path = "runtime/tools_goals.rs"]
mod tools_goals;
#[path = "runtime/turn_core.rs"]
mod turn_core;
