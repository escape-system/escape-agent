//! Live mid-turn delivery.
//!
//! Inbox rows with `delivery: "steer"` must promote at step boundaries of a
//! running turn, while `delivery: "queue"` rows wait for the next execution.
//! Everything runs offline against a temp SQLite database and a scripted model
//! whose steps can hold a gate so the test can enqueue between them.

#[path = "steer_live/detach.rs"]
mod detach;
#[path = "steer_live/step_boundaries.rs"]
mod step_boundaries;
#[path = "steer_live/support.rs"]
mod support;
