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
        }
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
            handle.wake.notify_one();
            return;
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
    }

    async fn drain(&self, session: &str, handle: &Arc<ActorHandle>) -> Result<(), RuntimeError> {
        loop {
            if self.inner.shutdown.is_cancelled() {
                return Ok(());
            }
            let Some(item) = self.next_deliverable(session).await? else {
                return Ok(());
            };
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
}
