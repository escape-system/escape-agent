//! Permanent memory: hybrid retrieval, the rendered instruction block, the
//! four `memory_*` tools and the asynchronous scribe.
//!
//! Memory rows live in `oc-store` (FTS5, soft delete, scoped search, pinned
//! rows). This crate adds the parts the model sees:
//!
//! - [`MemoryService::retrieve`] merges the caller's visible scopes, ranks
//!   pinned rows first, removes exact and near duplicates and renders a
//!   `<memory scope="…">` block trimmed to a token budget.
//! - [`MemoryService::write`] enforces the 500-character cap, the secret
//!   detector and the documented defaults (`importance = 3`,
//!   `confidence = 0.7`, `mem_…` ids).
//! - [`MemoryTools::registry`] exposes `memory_write`, `memory_search`,
//!   `memory_forget` and `memory_list` as [`oc_tools::Tool`]s.
//! - [`run_scribe`] prompts a host-provided [`ScribeModel`], parses the strict
//!   JSON array and filters the candidates.
//!
//! The crate does not depend on `oc-runtime` or `oc-server`; token counts use a
//! local `chars / 4` estimate.

mod error;
mod render;
mod scribe;
mod secret;
mod service;
mod tools;

pub use error::{MemoryError, SecretRule};
pub use render::{
    CHARS_PER_TOKEN, DEFAULT_TOKEN_BUDGET, estimate_tokens, near_duplicate, token_set,
};
pub use scribe::{
    BoxFuture, MAX_CANDIDATES, MAX_TAGS, MIN_CONFIDENCE, MIN_TEXT_CHARS, MemoryCandidate,
    ScribeModel, filter_candidates, parse_candidates, run_scribe, scribe_prompt,
};
pub use secret::{detect_secret, ensure_no_secret};
pub use service::{
    DEFAULT_CONFIDENCE, DEFAULT_IMPORTANCE, LIST_DEFAULT, LIST_MAX, ListRequest, MAX_SEARCH_LIMIT,
    MIN_SEARCH_LIMIT, MemoryBlock, MemoryService, NewMemoryInput, PINNED_IMPORTANCE,
    RetrieveRequest, TEXT_CAP,
};
pub use tools::{MemoryForgetTool, MemoryListTool, MemorySearchTool, MemoryTools, MemoryWriteTool};
