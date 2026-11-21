//! W4B I3 — honest gap note (not a passing "fix" test).
//!
//! No concrete per-turn isolation bug could be proven within the owned file
//! `crates/oc-runtime/src/prompt/transcript.rs`. Stored assistant messages use
//! `AssistantPart::{Text, Reasoning, Tool}` only; `assistant_messages()` emits
//! each tool part as a paired call + result in the same rebuilt message, so an
//! "orphan tool result" cannot be produced from real stored data. Peer messages
//! and compaction `keepID` replay are rebuilt deterministically from this turn's
//! slice; there is no mutable per-turn cursor/index state in transcript.rs that
//! could leak across turns. The SDK 0.1.24 state-isolation fix concerns its own
//! `ChatResponse`/`Conversation` cursors and usage snapshots, which do not exist
//! in this file. See box wave4-turns for the full gap analysis.
//!
//! This file intentionally contains NO tests: shipping a synthetic regression
//! test that passes both before and after a non-fix would be fake success.
