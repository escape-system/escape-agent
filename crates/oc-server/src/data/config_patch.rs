//! `PATCH /api/experimental/config`.
//!
//! Upstream only writes `shell`; we accept the additive agentic keys as well.
//! The write goes to the highest-precedence *global* config file, merges into
//! the existing document, and requires a restart to take effect because the
//! loaded `ConfigService` snapshot is immutable.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use jsonc_parser::ParseOptions;
use oc_types::ApiError;
use serde_json::{Map, Value};

use crate::AppState;
use crate::error::Respond;

static CONFIG_LOCK: Mutex<()> = Mutex::new(());
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

const ALLOWED: [&str; 8] = [
    "shell",
    "subagents",
    "observer",
    "goal",
    "memory",
    "lanes",
    "lsp_install",
    "events",
];

pub async fn patch(State(_state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(object) = body.as_object() else {
        return ApiError::bad_request("config patch must be a JSON object").respond();
    };
    if object.is_empty() {
        return ApiError::bad_request("config patch must not be empty").respond();
    }
    for key in object.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return ApiError::bad_request(format!("unsupported config key: {key}")).respond();
        }
    }

    let dir = global_dir();
    let object = object.clone();
    match tokio::task::spawn_blocking(move || patch_in(&dir, &object)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return error.respond(),
        Err(error) => {
            return ApiError::internal(format!("config patch task failed: {error}")).respond();
        }
    }

    let mut response = axum::http::StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        axum::http::HeaderName::from_static("x-opencode-restart"),
        HeaderValue::from_static("required"),
    );
    response
}

fn patch_in(dir: &std::path::Path, object: &Map<String, Value>) -> Result<PathBuf, ApiError> {
    update_document(dir, |document| {
        for (key, value) in object {
            if value.is_null() {
                document.remove(key);
            } else {
                document.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    })
}

/// Read, edit and atomically rewrite the highest-precedence global document.
///
/// The editor receives the whole root object, so nested writes (for example
/// `mcp.servers.<name>`) merge into the existing document instead of replacing
/// unrelated keys.
pub(crate) fn update_document<F>(dir: &std::path::Path, edit: F) -> Result<PathBuf, ApiError>
where
    F: FnOnce(&mut Map<String, Value>) -> Result<(), ApiError>,
{
    let _guard = CONFIG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = target_file(dir);
    let mut document = read_document(&path)?;
    edit(&mut document)?;
    write_document(&path, &document)?;
    Ok(path)
}

pub(crate) fn target_file(dir: &std::path::Path) -> PathBuf {
    if dir.join("opencode.jsonc").is_file() {
        return dir.join("opencode.jsonc");
    }
    if dir.join("opencode.json").is_file() {
        return dir.join("opencode.json");
    }
    dir.join("opencode.jsonc")
}

pub(crate) fn global_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENCODE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(dir).join("opencode");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/opencode");
    }
    PathBuf::from(".opencode")
}

fn read_document(path: &std::path::Path) -> Result<Map<String, Value>, ApiError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(ApiError::service_unavailable(format!(
                "cannot read config {}: {error}",
                path.display()
            )));
        }
    };
    let value: Value = jsonc_parser::parse_to_serde_value(&text, &ParseOptions::default())
        .map_err(|error| {
            ApiError::bad_request(format!("existing config is not valid JSONC: {error}"))
        })?;
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(ApiError::bad_request(
            "existing config is not a JSON object",
        )),
    }
}

fn write_document(path: &std::path::Path, document: &Map<String, Value>) -> Result<(), ApiError> {
    let Some(parent) = path.parent() else {
        return Err(ApiError::internal("config path has no parent"));
    };
    std::fs::create_dir_all(parent).map_err(|error| {
        ApiError::internal(format!(
            "create config directory {}: {error}",
            parent.display()
        ))
    })?;
    let bytes = serde_json::to_vec_pretty(&Value::Object(document.clone()))
        .map_err(|error| ApiError::internal(format!("encode config: {error}")))?;
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp.{}.{seq}", std::process::id()));
    if let Err(error) = std::fs::write(&temp, bytes) {
        let _ = std::fs::remove_file(&temp);
        return Err(ApiError::internal(format!("write config temp: {error}")));
    }
    let renamed = {
        #[cfg(not(windows))]
        {
            std::fs::rename(&temp, path)
        }
        #[cfg(windows)]
        {
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            std::fs::rename(&temp, path)
        }
    };
    if let Err(error) = renamed {
        let _ = std::fs::remove_file(&temp);
        return Err(ApiError::internal(format!("install config: {error}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_a_new_global_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = patch_in(
            dir.path(),
            json!({"observer": {"enabled": true}, "lanes": {"cargo": 1}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(path.ends_with("opencode.jsonc"));
        let value: Value = jsonc_parser::parse_to_serde_value(
            &std::fs::read_to_string(path).unwrap(),
            &ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(value["observer"]["enabled"], true);
        assert_eq!(value["lanes"]["cargo"], 1);
    }

    #[test]
    fn merges_into_existing_jsonc_and_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("opencode.jsonc"),
            "{\n // keep\n \"model\": \"fake/model\",\n \"observer\": {\"tick_ms\": 1000,},\n}",
        )
        .unwrap();
        patch_in(
            dir.path(),
            json!({"observer": {"enabled": false}}).as_object().unwrap(),
        )
        .unwrap();
        let value: Value = jsonc_parser::parse_to_serde_value(
            &std::fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap(),
            &ParseOptions::default(),
        )
        .unwrap();
        assert_eq!(value["model"], "fake/model");
        assert_eq!(value["observer"]["enabled"], false, "whole-value replace");
        assert!(value["observer"].get("tick_ms").is_none());
    }

    #[test]
    fn null_removes_a_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("opencode.json"),
            r#"{"shell":"bash","memory":{}}"#,
        )
        .unwrap();
        patch_in(dir.path(), json!({"memory": null}).as_object().unwrap()).unwrap();
        let value: Value = jsonc_parser::parse_to_serde_value(
            &std::fs::read_to_string(dir.path().join("opencode.json")).unwrap(),
            &ParseOptions::default(),
        )
        .unwrap();
        assert!(value.get("memory").is_none());
        assert_eq!(value["shell"], "bash");
    }

    #[test]
    fn invalid_jsonc_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("opencode.jsonc"), "{ broken").unwrap();
        let error =
            patch_in(dir.path(), json!({"shell": "bash"}).as_object().unwrap()).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap(),
            "{ broken"
        );
    }

    #[test]
    fn concurrent_updates_preserve_all_keys_without_temp_file_races() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for idx in 0..8 {
                let path = dir.path();
                scope.spawn(move || {
                    update_document(path, |document| {
                        document.insert(format!("k{idx}"), json!(idx));
                        Ok(())
                    })
                    .expect("concurrent update should succeed");
                });
            }
        });
        let value: Value = jsonc_parser::parse_to_serde_value(
            &std::fs::read_to_string(dir.path().join("opencode.jsonc")).unwrap(),
            &ParseOptions::default(),
        )
        .unwrap();
        for idx in 0..8 {
            assert_eq!(value[format!("k{idx}")], json!(idx));
        }
    }
}
