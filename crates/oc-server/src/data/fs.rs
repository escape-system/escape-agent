//! Filesystem routes: read one file, list one directory, find recursively.
//!
//! Reads and finds resolve their path against the request location and must
//! canonicalize *inside* that location. Parent components (`..`) are rejected
//! outright; symlinks that point outside the location fail the prefix check.
//! Violations answer the same `404 FileNotFoundError` as a missing file so the
//! route never reveals what exists outside the location. Directory *listing*
//! is the one exception: the web picker walks from the location to parents and
//! other drives, so `fs/list` resolves absolute or relative paths without
//! confinement and answers 404 only when the directory does not exist.
//!
//! `GET /api/fs/read/*` is the only route that returns raw bytes. It carries a
//! strong `sha256` `ETag` (`"<64 hex chars>"`), `Last-Modified`, `Accept-Ranges`
//! and single-range `206` support; multiple or malformed ranges fall back to the
//! full `200` response.
//!
//! Only regular files are readable: directories answer `400 IsDirectoryError`,
//! devices, FIFOs and sockets answer the same `404 FileNotFoundError` as a
//! missing file, so a blocking or endless source is never opened. The read is
//! streamed through a `MAX_READ_BYTES + 1` cap, and a file that grows past the
//! cap after the metadata check answers `413 RequestTooLargeError`.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LAST_MODIFIED,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use ignore::WalkBuilder;
use oc_types::{ApiError, LocationEnvelope, LocationRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::AppState;
use crate::data::location;
use crate::data::query_error;
use crate::data::session::LocationQuery;
use crate::error::ErrorResponse;

/// Largest file `fs/read` will open. Larger files get `413`.
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_FIND_LIMIT: usize = 100;
const MAX_FIND_LIMIT: usize = 1000;

/// One `FileSystem.Entry` as served by this server.
///
/// Upstream's entry is only `{path,type}`; the client needs name, size and
/// mtime to render a file tree without a second round trip, so this is
/// additive. `path` is absolute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FsEntryKind,
    pub size: u64,
    /// Epoch milliseconds, `0` when the platform has no mtime.
    pub modified: i64,
}

/// `FileSystem.Entry.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FsEntryKind {
    File,
    Directory,
    Symlink,
}

/// `GET /api/fs/read/*`
pub async fn read(
    State(state): State<AppState>,
    AxumPath(path): AxumPath<String>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Response, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let root = canonical_root(&resolved)?;
    let target = confined(&root, &path)?;

    let meta = fs::metadata(&target)
        .await
        .map_err(|_| ErrorResponse::from(file_not_found(&path)))?;
    if meta.file_type().is_dir() {
        return Err(ErrorResponse::from(is_directory(&path)));
    }
    if !meta.file_type().is_file() {
        // Devices, FIFOs and sockets can block forever or never end; they are
        // not addressable through this route.
        return Err(ErrorResponse::from(file_not_found(&path)));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(ErrorResponse::from(read_too_large(&path)));
    }

    // Stream instead of `fs::read`: the cap is enforced on the bytes actually
    // consumed, even if the file grows past its metadata length.
    let file = fs::File::open(&target)
        .await
        .map_err(|_| ErrorResponse::from(file_not_found(&path)))?;
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ErrorResponse::from(file_not_found(&path)))?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(ErrorResponse::from(read_too_large(&path)));
    }
    let etag = etag(&bytes);
    let modified = http_date(meta.modified().unwrap_or(UNIX_EPOCH));

    if etag_matches(&headers, &etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(ETAG, etag)
            .header(LAST_MODIFIED, modified)
            .body(Body::empty())
            .map_err(|error| ErrorResponse::from(internal(error)));
    }

    let content_type = mime_guess::from_path(&target)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    let len = bytes.len() as u64;
    let requested = headers
        .get("range")
        .and_then(|value| value.to_str().ok())
        .map_or(ByteRange::Full, |value| parse_range(value, len));

    let (status, content_range, body) = match requested {
        ByteRange::Full => (StatusCode::OK, None, bytes),
        ByteRange::Partial { start, end } => {
            let slice = match (usize::try_from(start), usize::try_from(end)) {
                (Ok(start), Ok(end)) => bytes.get(start..=end).unwrap_or(bytes.as_slice()),
                _ => bytes.as_slice(),
            }
            .to_vec();
            (
                StatusCode::PARTIAL_CONTENT,
                Some(format!("bytes {start}-{end}/{len}")),
                slice,
            )
        }
        ByteRange::Unsatisfiable => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(CONTENT_RANGE, format!("bytes */{len}"))
                .header(ACCEPT_RANGES, "bytes")
                .body(Body::empty())
                .map_err(|error| ErrorResponse::from(internal(error)));
        }
    };

    let mut builder = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(ETAG, etag)
        .header(LAST_MODIFIED, modified)
        .header(ACCEPT_RANGES, "bytes")
        .header(CACHE_CONTROL, "no-cache");
    if let Some(range) = content_range {
        builder = builder.header(CONTENT_RANGE, range);
    }
    builder
        .body(Body::from(body))
        .map_err(|error| ErrorResponse::from(internal(error)))
}

/// `GET /api/fs/list?path=`
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Vec<FsEntry>>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let root = canonical_root(&resolved)?;

    let target = match params.path.as_deref() {
        None | Some("") => root.clone(),
        Some(path) => navigate(&root, path)?,
    };

    let mut directory = fs::read_dir(&target)
        .await
        .map_err(|_| ErrorResponse::from(file_not_found(&target.to_string_lossy())))?;
    let mut entries = Vec::new();
    while let Some(entry) = directory
        .next_entry()
        .await
        .map_err(|error| ErrorResponse::from(internal(error)))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !is_listable(&meta) {
            continue;
        }
        entries.push(entry_for(name, &path, &meta));
    }
    entries.sort_by_cached_key(|entry| {
        (
            kind_rank(entry.kind),
            entry.name.to_lowercase(),
            entry.name.clone(),
        )
    });

    Ok(Json(LocationEnvelope::new(resolved.reference(), entries)))
}

/// `GET /api/fs/find?query=&type=&limit=`
pub async fn find(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<FindQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Vec<FsEntry>>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let root = canonical_root(&resolved)?;
    let kind = parse_find_type(params.kind.as_deref())?;
    let limit = match params.limit.as_deref() {
        None | Some("") => DEFAULT_FIND_LIMIT,
        Some(raw) => raw.trim().parse::<usize>().map_err(|_| {
            ErrorResponse::from(ApiError::new(
                400,
                "SchemaError",
                format!("Invalid find limit: {raw}"),
            ))
        })?,
    }
    .min(MAX_FIND_LIMIT);

    let query = params.query;
    let entries = tokio::task::spawn_blocking(move || walk(&root, &query, kind, limit))
        .await
        .map_err(|error| ErrorResponse::from(internal(error)))?;

    Ok(Json(LocationEnvelope::new(resolved.reference(), entries)))
}

/// `GET /api/fs/list` query.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    path: Option<String>,
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
}

/// `GET /api/fs/find` query.
#[derive(Debug, Deserialize)]
pub struct FindQuery {
    query: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    limit: Option<String>,
    #[serde(rename = "location[directory]")]
    directory: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindType {
    All,
    File,
    Directory,
}

fn parse_find_type(value: Option<&str>) -> Result<FindType, ErrorResponse> {
    match value.unwrap_or("all") {
        "all" | "" => Ok(FindType::All),
        "file" => Ok(FindType::File),
        "directory" => Ok(FindType::Directory),
        other => Err(ErrorResponse::from(ApiError::new(
            400,
            "SchemaError",
            format!("Unknown find type {other:?}; expected all, file or directory"),
        ))),
    }
}

fn walk(root: &Path, query: &str, kind: FindType, limit: usize) -> Vec<FsEntry> {
    if limit == 0 {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .require_git(false)
        .hidden(true)
        .follow_links(false);
    let mut entries = Vec::new();
    for entry in builder.build().flatten() {
        if entries.len() >= limit {
            break;
        }
        if entry.depth() == 0 {
            continue;
        }
        let file_type = entry.file_type();
        let selected = match kind {
            FindType::File => file_type.is_some_and(|value| value.is_file()),
            FindType::Directory => file_type.is_some_and(|value| value.is_dir()),
            FindType::All => true,
        };
        if !selected {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(path);
        if !relative.to_string_lossy().to_lowercase().contains(&needle) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !is_listable(&meta) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push(entry_for(name, path, &meta));
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

/// Whether an entry is one of the kinds this API models. Devices, FIFOs and
/// sockets are skipped rather than being misreported as files.
fn is_listable(meta: &std::fs::Metadata) -> bool {
    let kind = meta.file_type();
    kind.is_file() || kind.is_dir() || kind.is_symlink()
}

fn entry_for(name: String, path: &Path, meta: &std::fs::Metadata) -> FsEntry {
    let kind = if meta.file_type().is_symlink() {
        FsEntryKind::Symlink
    } else if meta.is_dir() {
        FsEntryKind::Directory
    } else {
        FsEntryKind::File
    };
    FsEntry {
        name,
        path: display_path(path),
        kind,
        size: if kind == FsEntryKind::Directory {
            0
        } else {
            meta.len()
        },
        modified: modified_millis(meta),
    }
}

/// Absolute path without Windows' `\\?\` verbatim prefix.
pub(crate) fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    }
}

fn kind_rank(kind: FsEntryKind) -> u8 {
    match kind {
        FsEntryKind::Directory => 0,
        FsEntryKind::File | FsEntryKind::Symlink => 1,
    }
}

fn modified_millis(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Canonical location root; a missing location is the documented 404.
pub(crate) fn canonical_root(
    resolved: &location::RequestLocation,
) -> Result<PathBuf, ErrorResponse> {
    std::fs::canonicalize(&resolved.directory)
        .map_err(|_| ErrorResponse::from(location_not_found(&resolved.reference())))
}

/// Resolve `raw` for directory navigation.
///
/// Listing may leave the location: an absolute path is used as-is, a relative
/// one is joined to `root`, and `..` components are allowed. The canonical
/// result must exist and be a directory; anything else is the same 404 as a
/// missing directory. Only `fs/list` uses this; reads and finds stay confined.
fn navigate(root: &Path, raw: &str) -> Result<PathBuf, ErrorResponse> {
    let requested = Path::new(raw);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let absolute = std::path::absolute(&joined).unwrap_or(joined);
    let canonical =
        std::fs::canonicalize(&absolute).map_err(|_| ErrorResponse::from(file_not_found(raw)))?;
    let meta =
        std::fs::metadata(&canonical).map_err(|_| ErrorResponse::from(file_not_found(raw)))?;
    if !meta.is_dir() {
        return Err(ErrorResponse::from(file_not_found(raw)));
    }
    Ok(canonical)
}

/// Resolve `raw` against `root` and require the canonical result to stay inside.
pub(crate) fn confined(root: &Path, raw: &str) -> Result<PathBuf, ErrorResponse> {
    let requested = Path::new(raw);
    if requested
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ErrorResponse::from(file_not_found(raw)));
    }
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let canonical =
        std::fs::canonicalize(&joined).map_err(|_| ErrorResponse::from(file_not_found(raw)))?;
    if !canonical.starts_with(root) {
        return Err(ErrorResponse::from(file_not_found(raw)));
    }
    Ok(canonical)
}

/// Resolve `raw` against `root` for a write, allowing the final component (and
/// any missing parents) to not exist yet.
///
/// The nearest existing ancestor must canonicalize inside `root`; an existing
/// target is canonicalized too, so a symlink pointing outside is rejected. The
/// caller must re-check the canonical parent after creating directories.
pub(crate) fn confined_target(root: &Path, raw: &str) -> Result<PathBuf, ErrorResponse> {
    if raw.trim().is_empty() {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "BadRequestError",
            "A file path is required",
        )));
    }
    let requested = Path::new(raw);
    if requested
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ErrorResponse::from(file_not_found(raw)));
    }
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    let mut ancestor = joined.as_path();
    let mut remainder = PathBuf::new();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return Err(ErrorResponse::from(file_not_found(raw)));
        };
        let Some(name) = ancestor.file_name() else {
            return Err(ErrorResponse::from(file_not_found(raw)));
        };
        let mut next = PathBuf::from(name);
        if !remainder.as_os_str().is_empty() {
            next.push(&remainder);
        }
        remainder = next;
        ancestor = parent;
    }

    let canonical_ancestor =
        std::fs::canonicalize(ancestor).map_err(|_| ErrorResponse::from(file_not_found(raw)))?;
    if !canonical_ancestor.starts_with(root) {
        return Err(ErrorResponse::from(file_not_found(raw)));
    }
    let target = if remainder.as_os_str().is_empty() {
        canonical_ancestor
    } else {
        canonical_ancestor.join(remainder)
    };
    if target.as_path() == root || target.file_name().is_none() {
        return Err(ErrorResponse::from(ApiError::new(
            400,
            "BadRequestError",
            "A file path is required",
        )));
    }
    if target.exists() {
        let canonical =
            std::fs::canonicalize(&target).map_err(|_| ErrorResponse::from(file_not_found(raw)))?;
        if !canonical.starts_with(root) {
            return Err(ErrorResponse::from(file_not_found(raw)));
        }
        return Ok(canonical);
    }
    Ok(target)
}

pub(crate) fn file_not_found(path: &str) -> ApiError {
    ApiError::not_found("FileNotFoundError", format!("File not found: {path}"))
        .with("path", serde_json::json!(path))
}

fn is_directory(path: &str) -> ApiError {
    ApiError::new(
        400,
        "IsDirectoryError",
        format!("Path is a directory: {path}"),
    )
    .with("path", serde_json::json!(path))
}

fn read_too_large(path: &str) -> ApiError {
    ApiError::new(
        413,
        "RequestTooLargeError",
        format!("File exceeds the {MAX_READ_BYTES}-byte read limit: {path}"),
    )
}

pub(crate) fn location_not_found(location: &LocationRef) -> ApiError {
    ApiError::not_found(
        "LocationNotFoundError",
        format!("Location not found: {}", location.directory),
    )
    .with(
        "location",
        serde_json::json!({"directory": location.directory, "project": location.project}),
    )
}

pub(crate) fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %error, "filesystem operation failed");
    ApiError::internal("Internal error")
}

fn etag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("\"{}\"", hex::encode(digest))
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate).trim() == etag
            })
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteRange {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

/// Parse a single byte range. Multi-range and malformed headers mean "send the
/// whole file", exactly as the upstream route documents.
fn parse_range(value: &str, len: u64) -> ByteRange {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    let mut ranges = spec.split(',');
    let Some(first) = ranges.next() else {
        return ByteRange::Full;
    };
    if ranges.next().is_some() {
        return ByteRange::Full;
    }
    let Some((start, end)) = first.trim().split_once('-') else {
        return ByteRange::Full;
    };
    let start = start.trim();
    let end = end.trim();

    if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return ByteRange::Full;
        };
        if suffix == 0 || len == 0 {
            return ByteRange::Unsatisfiable;
        }
        return ByteRange::Partial {
            start: len.saturating_sub(suffix),
            end: len - 1,
        };
    }

    let Ok(start) = start.parse::<u64>() else {
        return ByteRange::Full;
    };
    if start >= len {
        return ByteRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        len - 1
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return ByteRange::Full;
        };
        end.min(len - 1)
    };
    if start > end {
        return ByteRange::Unsatisfiable;
    }
    ByteRange::Partial { start, end }
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// RFC 7231 IMF-fixdate, e.g. `Thu, 01 Jan 1970 00:00:00 GMT`.
fn http_date(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_secs());
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let weekday = usize::try_from((days + 4).rem_euclid(7)).unwrap_or(0);
    let month_name = usize::try_from(month.saturating_sub(1)).unwrap_or(0);
    format!(
        "{}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday],
        MONTHS[month_name],
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60,
    )
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
    )
}
