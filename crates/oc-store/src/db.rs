//! Connection management: pragmas, migrations, the writer thread and the read pool.
//!
//! One writer connection lives on a dedicated OS thread; reads are spread over a
//! small pool of connections and executed on `spawn_blocking`. No SQLite call
//! runs on the async runtime's worker threads.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::StoreError;
use crate::schema::MIGRATIONS;

pub(crate) const DEFAULT_LIMIT: usize = 50;
pub(crate) const MAX_LIMIT: usize = 200;
const READ_POOL_SIZE: usize = 4;
const BUSY_TIMEOUT_MS: u64 = 5_000;
const CACHE_SIZE_KIB: i32 = -32_768;
const READ_RETRIES: u32 = 4;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

enum Command {
    Run(WriteJob),
    Shutdown,
}

struct Writer {
    commands: Mutex<std::sync::mpsc::Sender<Command>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Writer {
    fn send(&self, job: WriteJob) -> Result<(), StoreError> {
        self.commands
            .lock()
            .send(Command::Run(job))
            .map_err(|_| StoreError::Closed)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.commands.lock().send(Command::Shutdown);
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }
}

struct ReadPool {
    connections: Vec<Mutex<Connection>>,
    next: AtomicUsize,
}

impl ReadPool {
    fn execute<T>(
        &self,
        mut work: impl FnMut(&mut Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let index = self.next.fetch_add(1, AtomicOrdering::Relaxed) % self.connections.len();
        let mut guard = self.connections[index].lock();
        let mut attempt = 0;
        loop {
            match work(&mut guard) {
                Err(error) if attempt < READ_RETRIES && is_transient(&error) => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
                result => return result,
            }
        }
    }
}

pub(crate) struct Inner {
    writer: Writer,
    readers: Arc<ReadPool>,
    path: Option<PathBuf>,
    /// Exclusive per-database lock held for the lifetime of the server process.
    /// One SQLite database belongs to one server; a second process is rejected
    /// instead of racing boot recovery against the live process.
    _lock: Option<std::fs::File>,
}

/// Handle to the SQLite database.
///
/// Cloning is cheap and every clone shares the same writer thread and read pool.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("path", &self.inner.path)
            .finish()
    }
}

impl Store {
    /// Opens (creating when missing) a database file.
    ///
    /// Parent directories are created, the connection is put into WAL mode and
    /// embedded migrations run before the store becomes usable.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be created, a pragma fails, a migration has
    /// drifted from its recorded checksum, or the writer thread cannot start.
    ///
    /// # Example
    ///
    /// ```
    /// use oc_store::{NewSession, Store};
    ///
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), oc_store::StoreError> {
    /// let path = std::env::temp_dir().join(format!("oc-store-doc-{}.db", std::process::id()));
    /// let store = Store::open(&path).await?;
    ///
    /// let session = store
    ///     .create_session(NewSession {
    ///         id: "ses_example".to_owned(),
    ///         parent_id: None,
    ///         project_id: "prj_example".to_owned(),
    ///         directory: "/tmp/example".to_owned(),
    ///         title: Some("example".to_owned()),
    ///         agent: None,
    ///         model: None,
    ///         metadata: None,
    ///         permissions: None,
    ///         time_created: 1_700_000_000_000,
    ///     })
    ///     .await?;
    /// assert_eq!(session["id"], "ses_example");
    ///
    /// drop(store);
    /// let _ = std::fs::remove_file(&path);
    /// let _ = std::fs::remove_file(path.with_extension("db-wal"));
    /// let _ = std::fs::remove_file(path.with_extension("db-shm"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        tokio::task::spawn_blocking(move || open_file(path))
            .await
            .map_err(|error| StoreError::Task(error.to_string()))?
    }

    /// Opens a private, shared-cache in-memory database. Intended for tests.
    pub async fn open_memory() -> Result<Self, StoreError> {
        tokio::task::spawn_blocking(open_memory)
            .await
            .map_err(|error| StoreError::Task(error.to_string()))?
    }

    /// The database file, or `None` for an in-memory store.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub(crate) async fn write<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.inner
            .writer
            .send(Box::new(move |connection: &mut Connection| {
                let _ = reply.send(work(connection));
            }))?;
        match result.await {
            Ok(value) => value,
            Err(_) => Err(StoreError::Closed),
        }
    }

    pub(crate) async fn read<T, F>(&self, work: F) -> Result<T, StoreError>
    where
        F: FnMut(&mut Connection) -> Result<T, StoreError> + Send + 'static,
        T: Send + 'static,
    {
        let pool = Arc::clone(&self.inner.readers);
        tokio::task::spawn_blocking(move || pool.execute(work))
            .await
            .map_err(|error| StoreError::Task(error.to_string()))?
    }
}

/// Sidecar lock file path (`<db>.lock`).
fn lock_path(path: &Path) -> PathBuf {
    let mut raw = path.as_os_str().to_owned();
    raw.push(".lock");
    PathBuf::from(raw)
}

#[cfg(windows)]
fn acquire_lock(path: &Path) -> Result<std::fs::File, StoreError> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;

    let lock = lock_path(path);
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&lock)
    {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(32) => {
            return Err(StoreError::Conflict(format!(
                "database {} is already in use by another opencode server",
                path.display()
            )));
        }
        Err(error) => return Err(StoreError::Io(error)),
    };
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

#[cfg(not(windows))]
fn acquire_lock(path: &Path) -> Result<std::fs::File, StoreError> {
    use std::io::Write;

    let lock = lock_path(path);
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock)
        {
            Ok(mut file) => {
                write!(file, "{}", std::process::id())?;
                return Ok(file);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                let alive = std::fs::read_to_string(&lock)
                    .ok()
                    .and_then(|raw| raw.trim().parse::<u32>().ok())
                    .is_some_and(process_alive);
                if alive {
                    return Err(StoreError::Conflict(format!(
                        "database {} is already in use by another opencode server",
                        path.display()
                    )));
                }
                // The owner died without cleaning up (hard kill); take it over.
                let _ = std::fs::remove_file(&lock);
            }
            Err(error) => return Err(StoreError::Io(error)),
        }
    }
    Err(StoreError::Conflict(format!(
        "database {} is already in use by another opencode server",
        path.display()
    )))
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // `kill -0` performs the liveness check without unsafe libc calls.
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(true)
}

fn open_file(path: PathBuf) -> Result<Store, StoreError> {
    let existed = path.exists();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let lock = acquire_lock(&path)?;
    let mut writer_connection = Connection::open(&path)?;
    configure(&writer_connection, false)?;
    migrate(&mut writer_connection)?;
    let readers = open_readers(&path)?;
    let writer = spawn_writer(writer_connection);
    if !existed {
        restrict_permissions(&path);
    }
    Ok(Store {
        inner: Arc::new(Inner {
            writer,
            readers: Arc::new(readers),
            path: Some(path),
            _lock: Some(lock),
        }),
    })
}

fn open_memory() -> Result<Store, StoreError> {
    static MEMORY_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "file:oc-store-memory-{}-{}?mode=memory&cache=shared",
        std::process::id(),
        MEMORY_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
    );
    let mut writer_connection = Connection::open(&name)?;
    configure(&writer_connection, true)?;
    migrate(&mut writer_connection)?;
    let mut connections = Vec::with_capacity(READ_POOL_SIZE);
    for _ in 0..READ_POOL_SIZE {
        let connection = Connection::open(&name)?;
        configure(&connection, true)?;
        connections.push(Mutex::new(connection));
    }
    let writer = spawn_writer(writer_connection);
    Ok(Store {
        inner: Arc::new(Inner {
            writer,
            readers: Arc::new(ReadPool {
                connections,
                next: AtomicUsize::new(0),
            }),
            path: None,
            _lock: None,
        }),
    })
}

fn open_readers(path: &Path) -> Result<ReadPool, StoreError> {
    let mut connections = Vec::with_capacity(READ_POOL_SIZE);
    for _ in 0..READ_POOL_SIZE {
        let connection = Connection::open(path)?;
        configure(&connection, false)?;
        connections.push(Mutex::new(connection));
    }
    Ok(ReadPool {
        connections,
        next: AtomicUsize::new(0),
    })
}

fn configure(connection: &Connection, memory: bool) -> Result<(), StoreError> {
    connection.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
    let journal_mode = if memory { "MEMORY" } else { "WAL" };
    connection.execute_batch(&format!(
        "PRAGMA journal_mode={journal_mode};\n\
         PRAGMA synchronous=NORMAL;\n\
         PRAGMA cache_size={CACHE_SIZE_KIB};\n\
         PRAGMA temp_store=MEMORY;\n\
         PRAGMA foreign_keys=ON;"
    ))?;
    Ok(())
}

fn spawn_writer(mut connection: Connection) -> Writer {
    let (commands, receiver) = std::sync::mpsc::channel::<Command>();
    let handle = std::thread::Builder::new()
        .name("oc-store-writer".to_owned())
        .spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    Command::Run(job) => {
                        let outcome = catch_unwind(AssertUnwindSafe(|| job(&mut connection)));
                        if outcome.is_err() {
                            let _ = connection.execute_batch("ROLLBACK;");
                        }
                    }
                    Command::Shutdown => break,
                }
            }
        });
    Writer {
        commands: Mutex::new(commands),
        thread: Mutex::new(handle.ok()),
    }
}

fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS migration (
            id TEXT PRIMARY KEY,
            time_completed INTEGER,
            checksum TEXT
         );",
    )?;
    for migration in MIGRATIONS {
        let checksum = checksum(migration.sql);
        let recorded: Option<String> = connection
            .query_row(
                "SELECT checksum FROM migration WHERE id = ?1",
                [migration.id],
                |row| row.get(0),
            )
            .optional()?;
        match recorded {
            Some(found) if found != checksum => {
                return Err(StoreError::MigrationChecksum(migration.id.to_owned()));
            }
            Some(_) => {}
            None => {
                if let Some((table, column)) = migration.column_guard
                    && column_exists(connection, table, column)?
                {
                    connection.execute(
                        "INSERT INTO migration (id, time_completed, checksum)
                             VALUES (?1, ?2, ?3)",
                        params![migration.id, now_ms(), checksum],
                    )?;
                    continue;
                }
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute_batch(migration.sql)?;
                transaction.execute(
                    "INSERT INTO migration (id, time_completed, checksum) VALUES (?1, ?2, ?3)",
                    params![migration.id, now_ms(), checksum],
                )?;
                transaction.commit()?;
            }
        }
    }
    Ok(())
}

fn checksum(sql: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(sql.as_bytes());
    hex::encode(hasher.finalize())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2",
        params![table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for candidate in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = std::fs::set_permissions(&candidate, permissions);
        }
    }
}

#[cfg(unix)]
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Runs `work` inside an immediate transaction, committing on success.
pub(crate) fn in_tx<T>(
    connection: &mut Connection,
    work: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let value = work(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

/// Reserves and returns the next dense sequence number for an aggregate.
pub(crate) fn next_seq(
    transaction: &Transaction<'_>,
    aggregate_id: &str,
) -> Result<i64, StoreError> {
    let current: Option<i64> = transaction
        .query_row(
            "SELECT seq FROM event_sequence WHERE aggregate_id = ?1",
            [aggregate_id],
            |row| row.get(0),
        )
        .optional()?;
    let next = current.unwrap_or(0) + 1;
    transaction.execute(
        "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?1, ?2, NULL)
         ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq",
        params![aggregate_id, next],
    )?;
    Ok(next)
}

/// Bumps the session's `time_updated` watermark without regressing it.
pub(crate) fn touch_session(
    transaction: &Transaction<'_>,
    session_id: &str,
    time: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE session_v2
         SET time_updated = max(coalesce(time_updated, 0), ?2)
         WHERE id = ?1",
        params![session_id, time],
    )?;
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Generates a wire id with `prefix` and the canonical ascending body.
///
/// The body comes from the crate's ascending [`oc_types::EventId`] generator;
/// only the prefix differs, so ids sort by creation time like upstream's.
pub(crate) fn generate_id(prefix: &str) -> String {
    let event = oc_types::EventId::new();
    let body = event
        .as_str()
        .strip_prefix("evt_")
        .unwrap_or(event.as_str());
    format!("{prefix}{body}")
}

pub(crate) fn encode_json(value: &Value) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

/// Encodes a JSON column, mapping `null` and absence to SQL `NULL`.
pub(crate) fn encode_json_opt(value: Option<&Value>) -> Result<Option<String>, StoreError> {
    match value {
        Some(Value::Null) | None => Ok(None),
        Some(value) => Ok(Some(serde_json::to_string(value)?)),
    }
}

pub(crate) fn decode_json(text: &str) -> Result<Value, StoreError> {
    serde_json::from_str(text)
        .map_err(|error| StoreError::Corrupt(format!("stored json is invalid: {error}")))
}

fn is_transient(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            )
    )
}
