//! Location resolution shared by the data routes.
//!
//! Priority is the documented one: an explicit body/query deep object
//! (`location[directory]`), then the `x-opencode-directory` header, then the
//! server's working directory. Session-scoped routes never consult the request
//! location for events; they use the directory stored on the session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use oc_types::{EventLocation, LocationRef, ProjectRef, project_id_from_directory};
use percent_encoding::percent_decode_str;

use crate::AppState;

/// Header carrying the caller's working directory.
pub const DIRECTORY_HEADER: &str = "x-opencode-directory";

const MAX_CACHED_LOCATIONS: usize = 64;
const LOCATION_CACHE_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct CachedLocation {
    resolved: RequestLocation,
    cached_at: Instant,
}

/// Bounded short-TTL cache for resolved project locations so repeated API calls
/// avoid synchronous `canonicalize` + SHA-256 on async worker threads.
#[derive(Debug, Default)]
pub(crate) struct LocationCache {
    entries: Mutex<HashMap<(PathBuf, String), CachedLocation>>,
}

/// A request-scoped location plus its project reference.
#[derive(Debug, Clone)]
pub struct RequestLocation {
    /// Absolute directory as requested (no `\\?\` verbatim prefix).
    pub directory: String,
    /// Canonical directory used for the project id.
    pub canonical: String,
    /// Canonical project identity.
    pub project: ProjectRef,
}

impl RequestLocation {
    /// The `location` object embedded in `Location.response` envelopes.
    #[must_use]
    pub fn reference(&self) -> LocationRef {
        LocationRef {
            directory: self.directory.clone(),
            project: self.project.clone(),
        }
    }

    /// The location as carried by events.
    #[must_use]
    pub fn event(&self) -> EventLocation {
        EventLocation {
            directory: self.directory.clone(),
        }
    }

    /// Absolute path for filesystem checks and session rows.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.directory)
    }
}

/// Resolve the location for a request.
///
/// `explicit` (body) wins over `query` (`location[directory]`), the header is
/// the fallback, and the server directory is the last resort.
#[must_use]
pub fn resolve(
    state: &AppState,
    headers: &HeaderMap,
    explicit: Option<&str>,
    query: Option<&str>,
) -> RequestLocation {
    let raw = explicit
        .map(str::to_owned)
        .or_else(|| query.map(str::to_owned))
        .or_else(|| header_directory(headers));
    let key_raw = raw.as_deref().unwrap_or(".");
    let base = state.directory();

    let now = Instant::now();
    {
        let guard = state
            .location_cache()
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.get(&(base.to_path_buf(), key_raw.to_owned()))
            && now.saturating_duration_since(entry.cached_at) < LOCATION_CACHE_TTL
        {
            return entry.resolved.clone();
        }
    }

    let (resolved, canonicalized) = build_with_status(base, key_raw);
    if canonicalized {
        let mut guard = state
            .location_cache()
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() >= MAX_CACHED_LOCATIONS {
            guard.retain(|_, v| now.saturating_duration_since(v.cached_at) < LOCATION_CACHE_TTL);
            if guard.len() >= MAX_CACHED_LOCATIONS {
                guard.clear();
            }
        }
        guard.insert(
            (base.to_path_buf(), key_raw.to_owned()),
            CachedLocation {
                resolved: resolved.clone(),
                cached_at: now,
            },
        );
    }
    resolved
}

fn header_directory(headers: &HeaderMap) -> Option<String> {
    headers
        .get(DIRECTORY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| percent_decode_str(value).decode_utf8_lossy().into_owned())
}

/// Normalize `raw` against `base` and derive its project reference.
#[must_use]
pub fn build(base: &Path, raw: &str) -> RequestLocation {
    build_with_status(base, raw).0
}

fn build_with_status(base: &Path, raw: &str) -> (RequestLocation, bool) {
    let requested = PathBuf::from(raw);
    let absolute = if requested.is_absolute() {
        requested
    } else {
        base.join(requested)
    };
    let absolute = std::path::absolute(&absolute).unwrap_or(absolute);
    let directory = absolute.to_string_lossy().into_owned();
    let (canonical, canonicalized) = match std::fs::canonicalize(&absolute) {
        Ok(path) => (path.to_string_lossy().into_owned(), true),
        Err(_) => (directory.clone(), false),
    };
    let project = ProjectRef {
        id: project_id_from_directory(&canonical),
        directory: directory.clone(),
        canonical,
    };
    (
        RequestLocation {
            directory,
            canonical: project.canonical.clone(),
            project,
        },
        canonicalized,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn info() -> oc_types::ServerInfo {
        oc_types::ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 1,
            hostname: Some("127.0.0.1".to_owned()),
            urls: Vec::new(),
            paths: oc_types::ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        }
    }

    #[test]
    fn default_resolution_does_not_join_a_relative_directory_twice() {
        let base = PathBuf::from("relative-location-test");
        let state = crate::AppState::new(info(), None, base.clone());
        let resolved = resolve(&state, &HeaderMap::new(), None, None);
        let expected = std::path::absolute(&base).expect("absolute");
        assert_eq!(PathBuf::from(&resolved.directory), expected);
        assert!(!resolved.directory.ends_with(&format!(
            "relative-location-test{}relative-location-test",
            std::path::MAIN_SEPARATOR
        )));
    }
}
