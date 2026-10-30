//! Canonicalization of one configuration document, including V1 aliases.
//!
//! The output keeps unknown keys (docs/14: "Unknown keys in a config document
//! are preserved and ignored, never rejected") and drops keys that the V1
//! format retired, recording a warning for each.

mod agents;
mod commands;
mod compaction;
mod copy;
mod ctx;
mod experimental;
mod keys;
mod mcp;
mod model;
mod permissions;
mod policies;
mod providers;
mod resources;
mod root;
mod skills;

pub(crate) use model::{model_selection, model_string};
pub(crate) use root::normalize;
