//! Static web app serving: `web/dist` with SPA fallback, bounded in-memory
//! cache, pre-compressed gzip payloads, strong `ETag` / `304`, and exact cache rules.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG, IF_NONE_MATCH, VARY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use flate2::Compression;
use flate2::write::GzEncoder;
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};

use crate::AppState;

/// Maximum individual file size retained in [`StaticCache`] (1 MiB).
const MAX_CACHED_FILE_BYTES: usize = 1024 * 1024;
/// Maximum combined byte budget for cached raw + gzip bodies (12 MiB).
const MAX_CACHE_TOTAL_BYTES: usize = 12 * 1024 * 1024;
/// Maximum resolved path hints retained for fast lookup.
const MAX_PATH_HINTS: usize = 512;
/// Minimum body size before pre-compressing with gzip (matches `CompressionLayer`).
const GZIP_THRESHOLD_BYTES: usize = 1024;

#[derive(Debug, Clone)]
struct CachedStatic {
    len: u64,
    modified_nanos: Option<u128>,
    etag: HeaderValue,
    content_type: HeaderValue,
    cache_control: &'static str,
    raw: Bytes,
    gzip: Option<Bytes>,
}

impl CachedStatic {
    fn byte_size(&self) -> usize {
        self.raw.len() + self.gzip.as_ref().map_or(0, Bytes::len)
    }
}

#[derive(Debug, Default)]
struct CacheInner {
    canonical_dist: Option<(PathBuf, PathBuf)>,
    path_hints: HashMap<String, PathBuf>,
    path_hint_order: VecDeque<String>,
    entries: HashMap<PathBuf, Arc<CachedStatic>>,
    entry_order: VecDeque<PathBuf>,
    total_bytes: usize,
}

impl CacheInner {
    fn insert_hint(&mut self, key: String, file: PathBuf) {
        if self.path_hints.contains_key(&key) {
            return;
        }
        while self.path_hints.len() >= MAX_PATH_HINTS {
            if let Some(old) = self.path_hint_order.pop_front() {
                self.path_hints.remove(&old);
            } else {
                break;
            }
        }
        self.path_hint_order.push_back(key.clone());
        self.path_hints.insert(key, file);
    }

    fn insert_entry(&mut self, file: PathBuf, entry: Arc<CachedStatic>) {
        let size = entry.byte_size();
        if size > MAX_CACHE_TOTAL_BYTES {
            return;
        }
        if let Some(old) = self.entries.remove(&file) {
            self.total_bytes = self.total_bytes.saturating_sub(old.byte_size());
            self.entry_order.retain(|path| path != &file);
        }
        while self.total_bytes.saturating_add(size) > MAX_CACHE_TOTAL_BYTES {
            let Some(evict) = self.entry_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&evict) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.byte_size());
            }
        }
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.entry_order.push_back(file.clone());
        self.entries.insert(file, entry);
    }

    fn remove_entry(&mut self, file: &Path) {
        if let Some(old) = self.entries.remove(file) {
            self.total_bytes = self.total_bytes.saturating_sub(old.byte_size());
            self.entry_order.retain(|path| path != file);
        }
    }
}

/// Bounded in-memory static asset cache owned by [`AppState`].
#[derive(Debug, Default)]
pub(crate) struct StaticCache {
    inner: Mutex<CacheInner>,
}

/// Try to serve a static asset without request headers.
pub async fn serve(state: &AppState, method: &Method, path: &str) -> Option<Response> {
    serve_with_headers(state, method, path, &HeaderMap::new()).await
}

/// Try to serve a static asset with `If-None-Match` and `Accept-Encoding` support.
/// Returns `None` for non-GET/HEAD requests so the caller can emit the method error.
pub(crate) async fn serve_with_headers(
    state: &AppState,
    method: &Method,
    path: &str,
    req_headers: &HeaderMap,
) -> Option<Response> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    let dist = state.web_dist()?;

    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    if decoded.contains('\0') {
        return Some(not_found());
    }

    let relative = decoded.trim_start_matches('/');
    // Sourcemaps are Sentry-only artifacts, not part of the shipped dist
    // (`docs/13-ui.md`, `docs/24-ui-performance.md`); serving one would leak the
    // source tree, so 404 them even if a build leaves one on disk.
    if Path::new(relative)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("map"))
    {
        return Some(not_found());
    }

    let is_asset = relative.starts_with("_assets/") || relative.starts_with("icons/");
    let cache = state.static_cache();
    let (file, meta) = match resolve_static_target(cache, dist, relative, is_asset).await {
        ResolveOutcome::Found(file, meta) => (file, meta),
        ResolveOutcome::NotFound => return Some(not_found()),
        ResolveOutcome::NotReady => return Some(not_ready()),
    };

    let stamp = stamp_of(&meta);
    let cached = {
        let guard = cache
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entries
            .get(&file)
            .filter(|entry| entry.len == stamp.0 && entry.modified_nanos == stamp.1)
            .cloned()
    };

    if let Some(entry) = cached {
        return Some(build_cached_response(method, req_headers, &entry));
    }

    let Ok(bytes) = tokio::fs::read(&file).await else {
        let mut guard = cache
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove_entry(&file);
        guard.path_hints.remove(relative);
        return Some(if is_asset { not_found() } else { not_ready() });
    };

    let cache_control = cache_control_for(&file, is_asset);

    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    let content_type = HeaderValue::from_str(mime.as_ref())
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let etag_header = compute_etag(&bytes);
    let raw = Bytes::from(bytes);

    if raw.len() <= MAX_CACHED_FILE_BYTES {
        let gzip = if raw.len() > GZIP_THRESHOLD_BYTES && is_compressible(mime.as_ref()) {
            compress_gzip(&raw)
        } else {
            None
        };
        let entry = Arc::new(CachedStatic {
            len: stamp.0,
            modified_nanos: stamp.1,
            etag: etag_header,
            content_type,
            cache_control,
            raw,
            gzip,
        });
        cache
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert_entry(file, Arc::clone(&entry));
        return Some(build_cached_response(method, req_headers, &entry));
    }

    if etag_matches(req_headers, &etag_header) {
        return Some(not_modified(&etag_header, cache_control));
    }

    let mut response = if *method == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        Body::from(raw).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type);
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
    headers.insert(ETAG, etag_header);
    Some(response)
}

fn cache_control_for(file: &Path, is_asset: bool) -> &'static str {
    let is_index = file.file_name().is_some_and(|name| name == "index.html");
    let is_service_worker = file
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "sw.js" || name == "registerSW.js");
    if is_index || is_service_worker || !is_asset {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    }
}

enum ResolveOutcome {
    Found(PathBuf, std::fs::Metadata),
    NotFound,
    NotReady,
}

async fn resolve_static_target(
    cache: &StaticCache,
    dist: &Path,
    relative: &str,
    is_asset: bool,
) -> ResolveOutcome {
    let hinted_file = {
        let guard = cache
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.path_hints.get(relative).cloned()
    };

    let had_hint = hinted_file.is_some();
    if let Some(hint) = hinted_file
        && let Ok(meta) = tokio::fs::metadata(&hint).await
        && meta.is_file()
    {
        return ResolveOutcome::Found(hint, meta);
    }

    if had_hint {
        let mut guard = cache
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.path_hints.remove(relative);
    }
    let Some(canonical_dist) = canonical_dist_for(cache, dist).await else {
        return ResolveOutcome::NotReady;
    };
    let requested = if relative.is_empty() {
        dist.join("index.html")
    } else {
        dist.join(relative)
    };
    let file = match resolve_within_canonical(&canonical_dist, &requested).await {
        Some(resolved) => resolved,
        None if is_asset => return ResolveOutcome::NotFound,
        None => match resolve_within_canonical(&canonical_dist, &dist.join("index.html")).await {
            Some(index) => index,
            None => return ResolveOutcome::NotReady,
        },
    };
    let Ok(meta) = tokio::fs::metadata(&file).await else {
        return ResolveOutcome::NotFound;
    };
    if !meta.is_file() {
        return ResolveOutcome::NotFound;
    }
    let mut guard = cache
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.insert_hint(relative.to_owned(), file.clone());
    ResolveOutcome::Found(file, meta)
}

fn build_cached_response(
    method: &Method,
    req_headers: &HeaderMap,
    entry: &CachedStatic,
) -> Response {
    if etag_matches(req_headers, &entry.etag) {
        return not_modified(&entry.etag, entry.cache_control);
    }

    let send_gzip = *method == Method::GET
        && entry.gzip.is_some()
        && accepts_gzip(req_headers)
        && !prefers_brotli_over_gzip(req_headers);

    let mut response = if *method == Method::HEAD {
        StatusCode::OK.into_response()
    } else if send_gzip && let Some(gz) = &entry.gzip {
        Body::from_stream(futures_util::stream::once(std::future::ready(Ok::<
            _,
            std::convert::Infallible,
        >(
            gz.clone()
        ))))
        .into_response()
    } else {
        Body::from(entry.raw.clone()).into_response()
    };

    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, entry.content_type.clone());
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(entry.cache_control));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // Every static response is negotiated by `accept-encoding` (either pre-compressed
    // here or via the compression layer), so caches must key on it.
    headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
    headers.insert(ETAG, entry.etag.clone());
    if send_gzip {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    response
}

fn not_modified(etag: &HeaderValue, cache_control: &'static str) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(VARY, HeaderValue::from_static("accept-encoding"));
    headers.insert(ETAG, etag.clone());
    response
}

async fn canonical_dist_for(cache: &StaticCache, dist: &Path) -> Option<PathBuf> {
    {
        let guard = cache
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_key, canonical)) = &guard.canonical_dist
            && cached_key == dist
        {
            return Some(canonical.clone());
        }
    }
    let canonical = tokio::fs::canonicalize(dist).await.ok()?;
    let mut guard = cache
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.canonical_dist = Some((dist.to_path_buf(), canonical.clone()));
    Some(canonical)
}

/// Canonicalize `candidate` and require the result to stay under `canonical_dist`.
async fn resolve_within_canonical(canonical_dist: &Path, candidate: &Path) -> Option<PathBuf> {
    let canonical = tokio::fs::canonicalize(candidate).await.ok()?;
    canonical.starts_with(canonical_dist).then_some(canonical)
}

fn stamp_of(meta: &std::fs::Metadata) -> (u64, Option<u128>) {
    let modified = meta
        .modified()
        .ok()
        .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    (meta.len(), modified)
}

fn compute_etag(bytes: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(bytes);
    let tag = format!("\"{}\"", hex::encode(&digest[..16]));
    HeaderValue::from_str(&tag).unwrap_or(HeaderValue::from_static("\"0\""))
}

fn etag_matches(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let Ok(expected) = etag.to_str() else {
        return false;
    };
    headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == "*"
                    || candidate.strip_prefix("W/").unwrap_or(candidate).trim() == expected
            })
        })
}

fn is_compressible(mime: &str) -> bool {
    !(mime.starts_with("image/")
        || mime.starts_with("font/")
        || mime.starts_with("audio/")
        || mime.starts_with("video/"))
}

fn compress_gzip(raw: &[u8]) -> Option<Bytes> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(raw.len() / 3), Compression::fast());
    encoder.write_all(raw).ok()?;
    let compressed = encoder.finish().ok()?;
    Some(Bytes::from(compressed))
}

fn accepts_gzip(headers: &HeaderMap) -> bool {
    encoding_quality(headers, "gzip").is_some_and(|q| q > 0.0)
}

fn prefers_brotli_over_gzip(headers: &HeaderMap) -> bool {
    let Some(br_q) = encoding_quality(headers, "br") else {
        return false;
    };
    if br_q <= 0.0 {
        return false;
    }
    let gzip_q = encoding_quality(headers, "gzip").unwrap_or(0.0);
    br_q > gzip_q
}

fn encoding_quality(headers: &HeaderMap, target: &str) -> Option<f32> {
    let raw = headers.get(ACCEPT_ENCODING)?.to_str().ok()?;
    for item in raw.split(',') {
        let mut parts = item.trim().split(';');
        let Some(name) = parts.next().map(str::trim) else {
            continue;
        };
        if !name.eq_ignore_ascii_case(target) {
            continue;
        }
        let mut quality = 1.0_f32;
        for param in parts {
            if let Some(q_val) = param.trim().strip_prefix("q=")
                && let Ok(parsed) = q_val.trim().parse::<f32>()
            {
                quality = parsed;
            }
        }
        return Some(quality);
    }
    None
}

fn not_found() -> Response {
    let mut response = (StatusCode::NOT_FOUND, "").into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn not_ready() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        "Web UI is not built. Run ui/build.sh (or set OC_WEB_DIST) and restart.\n",
    )
        .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
