//! Integration tests for the extended store APIs: forms, saved permissions,
//! instruction entries, session revert/viewed maintenance and lane tickets.
//!
//! Every test uses a private in-memory database; no network or clock
//! assumptions beyond monotonic ids/order keys supplied by the test.

#![allow(clippy::too_many_lines)] // scenario tests read top-to-bottom on purpose

mod credentials;
mod forms;
mod instructions;
mod lanes;
mod saved_permissions;
mod schedule_crud;
mod schedule_fire;
mod schedule_parse;
mod schedule_validation;
mod snapshots;
mod support;
mod watermarks;
mod worktrees;
