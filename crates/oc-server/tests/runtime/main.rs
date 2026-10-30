//! End-to-end tests for the server ↔ runtime boundary.
//!
//! Every test drives the real router with a scripted [`ModelClient`], a temp
//! SQLite store and a temp config tree. No HTTP request leaves the process.

mod admission;
mod cancel_delete;
mod detach;
mod permissions;
mod support;
mod turn;
