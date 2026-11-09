//! Session runtime: actor-per-session turn loop.
//!
//! Phase 2 builds the execution engine on top of the pure primitives:
//! expectation contract, instruction prefix, token estimates and compaction
//! templates.
//!
//! # Execution model
//!
//! One actor exists per session. The server owns prompt admission: it writes
//! the inbox row and user message, then calls [`Runtime::notify`]. The actor
//! promotes one deliverable inbox row, loads the session/agent/model/tools,
//! runs the step loop, and settles `session.execution.*` before releasing the
//! (process-local) claim. `start` repairs rows left by a crashed process and
//! wakes sessions with pending input.
//!
//! Durable state is written before the event that describes it. Assistant
//! messages are appended once per step and rewritten at text end, tool call,
//! tool result and step end; assistant text deltas are never persisted.
//! Instruction deltas are the exception: `session.instructions.updated` is
//! stored in the session log (durable, v2) before it is published.
//!
//! # Default prompt
//!
//! When the selected agent has no `system` field the runtime uses
//! [`prompt::default_system_prompt`]: a concise coding-agent prompt that lists
//! the filtered tool names, the working directory and the platform, and
//! explains the `expectation` contract. `agent.system` replaces it; the
//! instruction builtins (date and `<env>`) are always appended as a second
//! system part.
//!
//! # Model adapter
//!
//! [`RuntimeConfig::client`] is a real [`oc_llm::SharedLlmClient`]. It is
//! adapted into [`ModelClient`] by [`LlmClientAdapter`]. Tests construct the
//! runtime with [`Runtime::new_with_model_client`] and a scripted client, so
//! no HTTP request is made offline.
//!
//! # Host wiring
//!
//! Memory is optional and additive; `RuntimeConfig` is unchanged.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use oc_runtime::{Runtime, RuntimeConfig};
//! # fn wire(config: RuntimeConfig) -> Runtime {
//! let memory = Arc::new(oc_memory::MemoryService::new(Arc::clone(&config.store)));
//! Runtime::new(config).with_memory(memory)
//! # }
//! ```
//!
//! The CLI builds one [`oc_memory::MemoryService`] over the same
//! [`oc_store::Store`] as the runtime and attaches it with
//! [`Runtime::with_memory`]. The service supplies the `oc/memory` instruction
//! source and the four `memory_*` tools merged into the per-session registry.
//!
//! The scribe model is host-owned: adapt an `oc-llm` one-shot completion to
//! [`oc_memory::ScribeModel`] and pass it to [`Runtime::with_scribe`]. Until
//! that adapter exists the runtime simply skips the scribe.
//!
//! # Subagent tools
//!
//! Every session turn merges [`SubagentTools`] into the registry after the
//! memory tools: `subagent`, `talk`, `roster`, `session_write` and `digest`.
//! Spawning, messaging and writing are decided by `oc_subagents::Policy` with
//! the `subagents` config section; spawn and sibling rate budgets plus the
//! digest cache live in the runtime. Background children report back through a
//! synthetic parent message carrying `metadata.source="subagent"`.
//!
//! # Goal, skill and lane tools
//!
//! Every turn also merges the runtime-owned tool sets after the subagent
//! tools: [`goal::GoalTools`] (`goal_set`, `goal_update`, `goal_complete`,
//! `goal_status`), [`skill::SkillTools`] (`skill`, `skill_write`,
//! `skill_lint`) and [`lane::LaneTools`] (`lane`, plus a lane-aware `shell`
//! that shadows the built-in). The lane tools dispatch through the
//! `oc_lanes::LaneService` attached with [`Runtime::with_lanes`].
//!
//! Hosts add their own dynamic tools with [`Runtime::add_tool_provider`];
//! providers are merged between [`RuntimeConfig::tools`] and the memory tools.

pub mod compaction;
mod context_box;
pub mod expectation;
pub mod goal;
mod goal_store;
pub mod instructions;
pub mod lane;
mod observer;
pub mod prompt;
mod recover;
mod runtime;
pub mod skill;
mod snapshot;
pub mod subagent;
pub mod tokens;
mod turn;

pub use compaction::{CompactionConfig, CompactionResult, SUMMARY_SECTIONS, summary_prompt};
pub use context_box::{BOX_READ_CHUNK, BoxTools, MAX_BOX_WRITE_CHARS, render_box_index};
pub use expectation::{Expectation, ExpectationError, extract, inject};
pub use goal::GoalTools;
pub use goal_store::StoreGoalStore;
pub use instructions::{
    InstructionDelta, InstructionSet, InstructionSource, SourceKind, hash_text, render,
};
pub use lane::{LaneTools, ProcessLaneRunner};
pub use observer::{ObserverBinding, RepairChannel, RepairCommand};
pub use prompt::{
    InstructionAssembly, InstructionContext, MAX_STEPS_PROMPT, assemble_instructions,
    default_system_prompt, instruction_sources,
};
pub use runtime::{
    EventSink, LlmClientAdapter, ModelClient, PermissionAsk, PermissionDecision, PermissionHandler,
    Runtime, RuntimeConfig, RuntimeError, ToolProvider,
};
pub use skill::SkillTools;
pub use subagent::SubagentTools;
pub use tokens::{
    IMAGE_TOKENS, PDF_TOKENS, PromptEstimate, compaction_ceiling, estimate_after_anchor,
    estimate_text, precompute_threshold, reserve, should_compact,
};
