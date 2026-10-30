//! Error type for the store.

/// Every failure the store can report.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite rejected a statement.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Filesystem failure (creating the data directory, for example).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A caller-supplied body could not be serialized.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// A row already exists with a conflicting identity.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The addressed row does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// A pagination cursor was not a valid encoded cursor.
    #[error("invalid cursor: {0}")]
    InvalidCursor(String),

    /// The caller passed a value the store cannot accept.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Stored data cannot be decoded (corrupt database or incompatible import).
    #[error("corrupt row: {0}")]
    Corrupt(String),

    /// The writer thread is gone; the store can no longer accept writes.
    #[error("store is closed")]
    Closed,

    /// A blocking task failed to complete.
    #[error("task failed: {0}")]
    Task(String),

    /// A migration's checksum does not match the embedded definition.
    #[error("migration {0} checksum mismatch")]
    MigrationChecksum(String),
}
