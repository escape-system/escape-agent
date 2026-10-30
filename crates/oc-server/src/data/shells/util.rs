//! Mutex, clock and capture-file plumbing shared by the shell modules.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use oc_types::ApiError;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Creates a capture file with an unpredictable suffix using `create_new`, so
/// a predictable name can never be truncated or hijacked through a symlink.
pub(super) fn create_capture(
    directory: &Path,
    id: &str,
) -> Result<(PathBuf, std::fs::File), ApiError> {
    for _ in 0..8 {
        let candidate = directory.join(format!("{id}-{:016x}.out", rand::random::<u64>()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ApiError::internal(format!(
                    "unable to capture shell output: {error}"
                )));
            }
        }
    }
    Err(ApiError::internal(
        "unable to allocate a unique shell capture file",
    ))
}
