use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use oc_observer::{GoalStore, ObserverEngine};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use crate::runtime::Runtime;

use super::repair::RepairCommand;

/// Observer collaborators attached to one [`Runtime`].
pub struct ObserverBinding {
    /// The observer loop.
    pub engine: Arc<ObserverEngine>,
    /// Goal persistence used by each tick.
    pub goals: Arc<dyn GoalStore>,
    /// Repairs produced by the engine's executor.
    pub repairs: UnboundedReceiver<RepairCommand>,
}

impl std::fmt::Debug for ObserverBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObserverBinding")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

/// Shared observer state owned by [`crate::runtime::Inner`].
pub(crate) struct ObserverHost {
    pub(crate) engine: Arc<ObserverEngine>,
    pub(super) goals: Arc<dyn GoalStore>,
    repairs: Mutex<Option<UnboundedReceiver<RepairCommand>>>,
    pub(super) tickers: Mutex<HashMap<String, JoinHandle<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for ObserverHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObserverHost")
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl ObserverHost {
    pub(crate) fn new(binding: ObserverBinding) -> Self {
        Self {
            engine: binding.engine,
            goals: binding.goals,
            repairs: Mutex::new(Some(binding.repairs)),
            tickers: Mutex::new(HashMap::new()),
            worker: Mutex::new(None),
        }
    }

    /// Start the repair worker exactly once.
    pub(crate) fn spawn_worker(self: &Arc<Self>, runtime: &Runtime) {
        let mut slot = self.worker.lock().expect("observer worker slot");
        if slot.is_some() {
            return;
        }
        let Some(receiver) = self.repairs.lock().expect("observer repairs slot").take() else {
            return;
        };
        let runtime = runtime.clone();
        *slot = Some(tokio::spawn(async move {
            runtime.observer_repair_loop(receiver).await;
        }));
    }

    fn drain_tickers(&self) {
        let handles: Vec<JoinHandle<()>> = {
            let mut tickers = self.tickers.lock().expect("observer tickers");
            tickers.drain().map(|(_, handle)| handle).collect()
        };
        for handle in handles {
            handle.abort();
        }
        if let Some(worker) = self.worker.lock().expect("observer worker slot").take() {
            worker.abort();
        }
    }
}

impl Runtime {
    /// Attach an observer engine, goal store and repair receiver.
    ///
    /// The binding is optional, so existing hosts keep working unchanged.
    /// [`Runtime::start`] spawns the repair worker; [`Runtime::notify`] spawns
    /// one ticker per root session.
    #[must_use]
    pub fn with_observer(self, binding: ObserverBinding) -> Self {
        let host = Arc::new(ObserverHost::new(binding));
        *self.inner.observer.lock().expect("observer slot") = Some(Arc::clone(&host));
        if self.inner.started.load(std::sync::atomic::Ordering::SeqCst) {
            host.spawn_worker(&self);
        }
        self
    }

    /// The attached observer engine, when configured.
    #[must_use]
    pub fn observer(&self) -> Option<Arc<ObserverEngine>> {
        self.observer_host().map(|host| Arc::clone(&host.engine))
    }

    pub(crate) fn observer_host(&self) -> Option<Arc<ObserverHost>> {
        self.inner.observer.lock().expect("observer slot").clone()
    }

    pub(crate) fn start_observer_worker(&self) {
        if let Some(host) = self.observer_host() {
            host.spawn_worker(self);
        }
    }

    pub(crate) fn stop_observer(&self) {
        if let Some(host) = self.observer_host() {
            host.drain_tickers();
        }
    }
}
