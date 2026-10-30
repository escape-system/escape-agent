//! Errors surfaced by the observer and its host-supplied collaborators.

/// A failure in a host-supplied collaborator.
///
/// The observer fails open: a model error never blocks the turn and never
/// triggers a repair. Executor errors are reported but do not retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObserverError {
    /// The observer model could not be reached or produced no text.
    #[error("observer model unavailable: {0}")]
    Model(String),
    /// The host could not apply a repair command.
    #[error("repair executor failed: {0}")]
    Executor(String),
    /// The host supplied an unusable configuration value.
    #[error("invalid observer configuration: {0}")]
    Config(String),
}
