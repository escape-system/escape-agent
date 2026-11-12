//! End-to-end test harness for the `opencode` binary.
//!
//! The crate provides three pieces:
//!
//! - [`FakeProvider`] — an in-process OpenAI/Anthropic/Gemini SSE provider
//!   that records every request body and replays a scripted response;
//! - [`ServerHarness`] — spawns `opencode serve` in an isolated temporary
//!   environment, parses the bound URL from stdout, and tears the process tree
//!   down on drop;
//! - [`TestConfig`] — builds the JSON configuration that points the catalog at
//!   the fake provider.
//!
//! Everything is offline: requests only reach loopback sockets owned by the
//! test process.

mod config;
mod fake;
mod harness;
mod http;

pub use config::TestConfig;
pub use fake::{FakeProvider, Script};
pub use harness::ServerHarness;
pub use http::{
    SseReader, assistant_text, create_session, events_after, json_request, parse_sse_frames,
    prompt, wait_for_execution,
};
