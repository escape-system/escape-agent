//! The observer loop: wake decisions, coalescing, repair budgets and actions.
//!
//! Everything the loop needs from the outside world is a trait: time
//! ([`Clock`]), the model ([`ObserverModel`]), repair execution
//! ([`RepairExecutor`]) and notice delivery ([`ObserverNotifier`]). Persistence
//! and HTTP live in the host; this module only decides.

mod actions;
mod budget;
mod goals;
mod notice;
mod repair;
mod run;
mod snapshot;
mod state;
mod traits;
mod wake;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::clock::Clock;
use crate::config::ObserverConfig;
use crate::registry::{ExpectationRegistry, RegistryCaps};

pub use budget::{BudgetState, TargetBudget};
pub use notice::{HostFlag, ObserverNotice, ObserverRun, RepairOutcome, SkipReason};
pub use traits::{BoxFuture, ObserverModel, ObserverNotifier, RepairExecutor};

use state::RootState;

/// Per-root observer engine.
pub struct ObserverEngine {
    pub(super) cfg: ObserverConfig,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) model: Arc<dyn ObserverModel>,
    pub(super) executor: Arc<dyn RepairExecutor>,
    pub(super) notifier: Arc<dyn ObserverNotifier>,
    pub(super) registry: ExpectationRegistry,
    pub(super) state: Mutex<HashMap<String, RootState>>,
}

impl fmt::Debug for ObserverEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObserverEngine")
            .field("cfg", &self.cfg)
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl ObserverEngine {
    /// Build an engine. The registry shares the supplied clock.
    #[must_use]
    pub fn new(
        cfg: ObserverConfig,
        clock: Arc<dyn Clock>,
        model: Arc<dyn ObserverModel>,
        executor: Arc<dyn RepairExecutor>,
        notifier: Arc<dyn ObserverNotifier>,
    ) -> Self {
        Self {
            registry: ExpectationRegistry::new(clock.clone()),
            cfg,
            clock,
            model,
            executor,
            notifier,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Override registry retention caps (must be called before feeding strips).
    #[must_use]
    pub fn with_registry_caps(mut self, caps: RegistryCaps) -> Self {
        self.registry = ExpectationRegistry::with_caps(self.clock.clone(), caps);
        self
    }

    /// The underlying strip registry.
    #[must_use]
    pub fn registry(&self) -> &ExpectationRegistry {
        &self.registry
    }

    /// Configuration in use.
    #[must_use]
    pub fn config(&self) -> &ObserverConfig {
        &self.cfg
    }
}
