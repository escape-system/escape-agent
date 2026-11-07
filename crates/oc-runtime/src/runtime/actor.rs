use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use oc_types::InterruptReason;

use super::{Runtime, RuntimeError};

pub(crate) struct ActorHandle {
    pub(crate) wake: Notify,
    pub(crate) idle: Notify,
    pub(crate) running: AtomicBool,
    /// Set by [`Runtime::request_background`] while an execution is running.
    /// The turn's foreground `subagent` wait consumes it and detaches; the
    /// actor resets it before every execution.
    pub(crate) background: AtomicBool,
    pub(crate) cancel: Mutex<Option<Arc<CancelToken>>>,
    pub(crate) join: Mutex<Option<JoinHandle<()>>>,
    /// Frozen instruction prefix for this session; deltas are computed against
    /// it so a turn never splices memory or skills per step.
    pub(crate) instructions: Mutex<Option<crate::instructions::InstructionSet>>,
    /// Terminal-event guard for the inbox item currently being drained.
    pub(crate) terminal: TerminalState,
}

impl ActorHandle {
    fn new() -> Self {
        Self {
            wake: Notify::new(),
            idle: Notify::new(),
            running: AtomicBool::new(false),
            background: AtomicBool::new(false),
            cancel: Mutex::new(None),
            join: Mutex::new(None),
            instructions: Mutex::new(None),
            terminal: TerminalState::default(),
        }
    }
}

/// Per-item terminal-event publication guard.
///
/// `owed` is armed by [`Runtime::drain`] when the item it is about to run is a
/// real execution (a `compaction` row runs outside one). [`Runtime::settle`]
/// clears it once the `session.execution.*` outcome event is durable.
///
/// The two `*_published` flags make the actor's failure path idempotent: a
/// settle can fail half-way, and re-running it must not publish the terminal
/// pair twice. `session.idle` and `session.status` are ephemeral, so the only
/// way to duplicate them is to reach them again after they were already
/// published; the flag makes that impossible.
#[derive(Debug, Default)]
pub(crate) struct TerminalState {
    owed: AtomicBool,
    idle_published: AtomicBool,
    outcome_published: AtomicBool,
}

impl TerminalState {
    /// Arm the guard for one inbox item. Called before the item runs.
    pub(crate) fn begin(&self, owed: bool) {
        self.owed.store(owed, Ordering::SeqCst);
        self.idle_published.store(false, Ordering::SeqCst);
        self.outcome_published.store(false, Ordering::SeqCst);
    }

    /// `true` while the current item still needs its terminal events.
    pub(crate) fn is_owed(&self) -> bool {
        self.owed.load(Ordering::SeqCst)
    }

    /// `true` once `session.idle` was published for the current item.
    pub(crate) fn idle_published(&self) -> bool {
        self.idle_published.load(Ordering::SeqCst)
    }

    /// `true` once the `session.execution.*` outcome was published.
    pub(crate) fn outcome_published(&self) -> bool {
        self.outcome_published.load(Ordering::SeqCst)
    }

    /// Records that the idle row and `session.idle` are committed/published.
    pub(crate) fn mark_idle(&self) {
        self.idle_published.store(true, Ordering::SeqCst);
    }

    /// Records that the execution outcome event is durable and clears `owed`.
    pub(crate) fn mark_outcome(&self) {
        self.outcome_published.store(true, Ordering::SeqCst);
        self.owed.store(false, Ordering::SeqCst);
    }
}

/// Cooperative cancellation token for one execution. The finalizer observes the
/// same token, so dropping a stream future cannot lose the terminal events.
pub(crate) struct CancelToken {
    cancelled: AtomicBool,
    reason: Mutex<InterruptReason>,
    notify: Notify,
    /// Shared with [`oc_tools::ToolContext`] so tools can stop helpers they
    /// spawned even after the tool future itself is dropped.
    pub(crate) token: tokio_util::sync::CancellationToken,
}

impl CancelToken {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            reason: Mutex::new(InterruptReason::User),
            notify: Notify::new(),
            token: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub(crate) fn cancel(&self, reason: InterruptReason) {
        *self.reason.lock().expect("cancel lock") = reason;
        self.cancelled.store(true, Ordering::SeqCst);
        self.token.cancel();
        self.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn reason(&self) -> InterruptReason {
        *self.reason.lock().expect("cancel lock")
    }

    pub(crate) async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl Runtime {
    pub(super) fn ensure_actor(&self, session_id: &str) {
        let mut actors = self.inner.actors.lock().expect("actor map");
        if let Some(handle) = actors.get(session_id) {
            // A finished task can never be woken again: `wake.notify_one()` on a
            // dead handle is lost and the session stays busy forever. Reap it and
            // spawn a fresh actor below instead of trusting the map entry.
            let finished = match handle.join.lock().expect("join slot").as_ref() {
                Some(join) => join.is_finished(),
                None => true,
            };
            if !finished {
                handle.wake.notify_one();
                return;
            }
            actors.remove(session_id);
        }
        let handle = Arc::new(ActorHandle::new());
        actors.insert(session_id.to_owned(), Arc::clone(&handle));
        let runtime = self.clone();
        let session = session_id.to_owned();
        let actor = Arc::clone(&handle);
        let join = tokio::spawn(async move {
            runtime.actor_loop(session, actor).await;
        });
        *handle.join.lock().expect("join slot") = Some(join);
    }

    async fn actor_loop(&self, session: String, handle: Arc<ActorHandle>) {
        loop {
            if self.inner.shutdown.is_cancelled() {
                break;
            }
            handle.running.store(true, Ordering::SeqCst);
            let result = self.drain(&session, &handle).await;
            handle.running.store(false, Ordering::SeqCst);
            handle.idle.notify_waiters();
            self.inner.idle.notify_waiters();
            if let Err(error) = result {
                tracing::error!(session = %session, error = %error, "session actor stopped");
                // The turn may have died before `settle` ran. Repair the rows
                // it left open and publish the terminal pair exactly once
                // before the actor exits.
                self.finalize_actor_failure(&session, &error, &handle.terminal)
                    .await;
                break;
            }
            if self.inner.shutdown.is_cancelled() {
                break;
            }
            tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => break,
                () = handle.wake.notified() => {}
            }
        }
        handle.running.store(false, Ordering::SeqCst);
        handle.idle.notify_waiters();
        self.inner.idle.notify_waiters();
        // Leave the map before returning: a caller that raced this actor's
        // death either found the live handle (its wake permit dies with us)
        // or the empty map (fresh actor). The durable check below then covers
        // the lost permit by respawning when input is already waiting.
        self.retire_actor(&session, &handle);
        self.respawn_if_input_waiting(&session).await;
    }

    async fn drain(&self, session: &str, handle: &Arc<ActorHandle>) -> Result<(), RuntimeError> {
        loop {
            if self.inner.shutdown.is_cancelled() {
                return Ok(());
            }
            let Some(item) = self.next_deliverable(session).await? else {
                return Ok(());
            };
            // Reset the terminal guard. `run_execution` arms it after it
            // publishes `session.execution.started`, so a row that never became
            // an execution (a compaction, a swallowed doorbell) never owes
            // `session.execution.*` events.
            handle.terminal.begin(false);
            let token = Arc::new(CancelToken::new());
            *handle.cancel.lock().expect("cancel slot") = Some(Arc::clone(&token));
            // A detach request applies to the execution that is about to run;
            // one left over from a previous item must never leak into it.
            handle.background.store(false, Ordering::SeqCst);
            let result = if item.get("type").and_then(Value::as_str) == Some("compaction") {
                self.run_manual_compaction(session, &item, &token).await
            } else {
                self.run_execution(session, &item, &token, handle).await
            };
            *handle.cancel.lock().expect("cancel slot") = None;
            result?;
        }
    }

    /// Removes this actor from the map when it is still the registered one.
    fn retire_actor(&self, session: &str, handle: &Arc<ActorHandle>) {
        let mut actors = self.inner.actors.lock().expect("actor map");
        if actors
            .get(session)
            .is_some_and(|current| Arc::ptr_eq(current, handle))
        {
            actors.remove(session);
        }
    }

    /// Starts a replacement actor when durable input is already waiting.
    ///
    /// A `notify` that lands while this actor is still alive stores a wake
    /// permit that is lost when the loop breaks. Without this check the inbox
    /// row would sit there until an unrelated request checks the session.
    async fn respawn_if_input_waiting(&self, session: &str) {
        if self.inner.shutdown.is_cancelled() {
            return;
        }
        match self.next_deliverable(session).await {
            Ok(Some(_)) => self.ensure_actor(session),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    session = %session,
                    error = %error,
                    "could not check for waiting input while retiring the actor"
                );
            }
        }
    }
}
