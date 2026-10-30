//! Build lanes: a shell-command classifier and a semaphore-enforced ticket
//! scheduler.
//!
//! The problem this crate solves: several agents run `cargo test` in the same
//! target directory; cargo serializes on its lock, agents block, turns stall
//! and tokens burn. The fix is a lane per resource key (`cargo:<workspace>`,
//! `node:<workspace>`, …) with one dispatcher and a semaphore. Concurrency is
//! enforced in Rust; it never depends on a model or a coordinator cooperating.
//!
//! # Shell integration
//!
//! The `shell` tool classifies a command before spawning it. Unclassified
//! commands keep the normal background/foreground shell path.
//!
//! ```ignore
//! let Some(request) = oc_lanes::classify(&command, &cwd) else {
//!     return shell_execute(command, cwd).await; // not a build/test command
//! };
//! if let Some(lane) = lanes.get(&request.lane).filter(|_| !request.lane.is_empty()) {
//!     let ticket = lanes.enqueue(request.lane.clone(), NewTicket {
//!         command: request.command,
//!         cwd,
//!         priority: 0,
//!         owner_session: Some(session_id.clone()),
//!         label: None,
//!     }).await?;
//!     // Return the ticket id immediately; progress/output arrive as
//!     // lane.queued|started|finished|cancelled events and shell events.
//!     return Ok(ShellToolResult::Ticket(ticket));
//! }
//! ```
//!
//! The tool does not await the process inline. It may call
//! [`LaneService::wait`] with the turn's remaining deadline, or return the
//! ticket and stream output through [`LaneEvents`]. A foreground call that
//! blocks on the semaphore would reproduce the problem the lane exists to
//! solve.
//!
//! The runner executes the command. It receives the running [`Ticket`] and can
//! resolve cancellation through the id:
//!
//! ```ignore
//! struct ShellRunner { tokens: CancellationTokens }
//! impl LaneRunner for ShellRunner {
//!     fn run<'a>(&'a self, ticket: &'a Ticket) -> BoxFuture<'a, RunnerResult> {
//!         Box::pin(async move {
//!             let token = self.tokens.get(&ticket.id).expect("live ticket");
//!             // spawn the process, select on token.cancelled()
//!             RunnerResult::success()
//!         })
//!     }
//! }
//! ```
//!
//! # Coordinator integration
//!
//! The built-in `build-master` coordinator (role `coordinator`) gets the `lane`
//! tool. It maps directly onto this API:
//!
//! | `lane` tool call | API |
//! |---|---|
//! | `lane.queue {lane, command, cwd, priority, owner}` | [`LaneService::enqueue`] |
//! | `lane.order {ticket, priority}` | [`LaneService::reorder`] |
//! | `lane.status {ticket}` | [`LaneService::ticket`] / [`LaneService::wait`] |
//! | `lane.status {lane}` | [`LaneService::status`] |
//! | `lane.cancel {ticket}` | [`LaneService::cancel`] |
//! | `lane.notify {session_id, text}` | session-level peer message, not a lane operation |
//!
//! The coordinator is an optimization layer: it chooses ordering and batching
//! (format → clippy → test, affected crate first). Safety remains the lane
//! semaphore, so a sibling that calls `shell cargo test` directly still cannot
//! run two cargo jobs in one workspace.
//!
//! # Cancellation
//!
//! Every ticket owns a [`tokio_util::sync::CancellationToken`]. Queued
//! cancellation removes the ticket from the priority queue; running
//! cancellation flips it to [`TicketState::Cancelled`] and cancels the token.
//! A runner that never observes its token keeps its semaphore slot until it
//! returns, so a buggy runner cannot silently raise concurrency. Running
//! tickets that already changed state never transition again: a late runner
//! result is ignored.
//!
//! # Result mapping
//!
//! A runner result with `exit == 0` becomes [`TicketState::Succeeded`].
//! Any non-zero exit becomes [`TicketState::Failed`] with the runner's message
//! (or a synthesized `command exited with status N`). A panicking runner is
//! caught and mapped to `Failed { exit: -1 }`. A runner may attach
//! [`RunnerResult::output_path`]; the service copies it onto the ticket in the
//! same watch update that publishes the terminal state, so a waiter always
//! observes both together. Cancellation wins: a cancelled ticket keeps no
//! output path, even when its runner returns one afterwards.
//!
//! # Classification
//!
//! [`classify`] parses with `shell-words` and never evaluates a shell. It
//! recognizes cargo (`build|test|clippy|nextest|run|check`), `rustc`, node
//! package managers (`test`, `run`, common scripts), `go test`, `pytest`
//! (directly or `python -m pytest`), `dotnet test`, `gradle`/`gradlew` and
//! `mvn`/`mvnw`. For `&&`, `||`, `|`, `;` and newline chains it returns one
//! request for the first recognized segment, because `shell` submits one
//! ticket per command string.

pub mod classifier;
pub mod config;
pub mod error;
pub mod service;
pub mod types;

pub use classifier::classify;
pub use config::{DEFAULT_CONCURRENCY, DEFAULT_MAX_QUEUE, LaneConfig};
pub use error::LaneError;
pub use oc_observer::{Clock, SystemClock};
pub use service::{BoxFuture, CancellationTokens, LaneEvents, LaneRunner, LaneService};
pub use types::{LaneRequest, LaneStatus, NewTicket, RunnerResult, Ticket, TicketId, TicketState};
