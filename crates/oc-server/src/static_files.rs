//! Static web app serving: `web/dist` with SPA fallback and exact cache rules.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;

use crate::AppState;

/// Try to serve a static asset. Returns `None` for non-GET/HEAD requests so the
/// caller can emit the method error.
pub async fn serve(state: &AppState, method: &Method, path: &str) -> Option<Response> {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return None;
    }
    let dist = state.web_dist()?;

    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    if decoded.contains('\0') {
        return Some(not_found());
    }

    let relative = decoded.trim_start_matches('/');
    let is_asset = relative.starts_with("_assets/") || relative.starts_with("icons/");
    let requested = if relative.is_empty() {
        dist.join("index.html")
    } else {
        dist.join(relative)
    };

    let file = match resolve_within(dist, &requested).await {
        Some(path) => path,
        None if is_asset => return Some(not_found()),
        None => match resolve_within(dist, &dist.join("index.html")).await {
            Some(index) => index,
            None => return Some(not_ready()),
        },
    };

    // A resolved directory (e.g. bare `/_assets`) is not a file. Reporting
    // "Web UI is not built" for it would be false: the dist exists, the path
    // just has no content.
    if !tokio::fs::metadata(&file)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Some(not_found());
    }

    let bytes = match tokio::fs::read(&file).await {
        Ok(bytes) => bytes,
        Err(_) if is_asset => return Some(not_found()),
        Err(_) => return Some(not_ready()),
    };

    let is_index = file.file_name().is_some_and(|name| name == "index.html");
    let is_service_worker = file
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "sw.js" || name == "registerSW.js");

    let cache = if is_index || is_service_worker {
        "no-cache"
    } else if is_asset {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };

    let mime = mime_guess::from_path(&file).first_or_octet_stream();
    let mut response = if *method == Method::HEAD {
        StatusCode::OK.into_response()
    } else {
        Body::from(bytes).into_response()
    };
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Some(response)
}

/// Canonicalize and require the result to stay under `dist`.
async fn resolve_within(dist: &Path, candidate: &Path) -> Option<PathBuf> {
    let canonical_dist = tokio::fs::canonicalize(dist).await.ok()?;
    let canonical = tokio::fs::canonicalize(candidate).await.ok()?;
    canonical.starts_with(&canonical_dist).then_some(canonical)
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
