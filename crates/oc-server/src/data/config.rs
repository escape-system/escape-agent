//! Config and agent routes.
//!
//! `GET /api/config` is a bare `Config.Entry[]`; agent routes use the location
//! envelope. Model and provider routes live in [`crate::data::model`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::HeaderMap;
use oc_types::{ApiError, LocationEnvelope};
use serde_json::{Map, Value, json};

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{query_error, require_config};
use crate::error::ErrorResponse;

/// `GET /api/config`
///
/// The config snapshot is loaded once for the server directory, so this route
/// cannot cheaply honour `?location[directory]=`; entries are the ones actually
/// in effect for this process.
pub async fn entries(State(state): State<AppState>) -> Result<Json<Value>, ErrorResponse> {
    let config = require_config(&state)?;
    let entries: Vec<Value> = config
        .entries()
        .iter()
        .map(oc_config::ConfigEntry::to_value)
        .collect();
    Ok(Json(Value::Array(entries)))
}

/// `GET /api/config/shell` — shells available for terminals and agent tool
/// calls as upstream `ConfigShell.Option[]` (`{path,name,acceptable}`).
pub async fn shells(State(state): State<AppState>) -> Json<Value> {
    let configured = state.config().map(|config| config.shell());
    Json(Value::Array(shell_options(configured.as_deref())))
}

fn shell_options(configured: Option<&str>) -> Vec<Value> {
    let mut candidates = Vec::new();
    if let Some(shell) = configured {
        candidates.push(shell.to_owned());
    }
    candidates.extend(platform_shells());

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let path = executable(candidate.trim())?;
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if !seen.insert(key) {
                return None;
            }
            let name = path.file_stem().map_or_else(
                || path.to_string_lossy().into_owned(),
                |stem| stem.to_string_lossy().to_ascii_lowercase(),
            );
            Some(json!({
                "path": path.to_string_lossy(),
                "name": name,
                "acceptable": compatible(&name),
            }))
        })
        .collect()
}

/// Shells the platform offers even when configuration never names one.
#[cfg(windows)]
fn platform_shells() -> Vec<String> {
    let mut shells = Vec::new();
    for candidate in ["pwsh", "powershell"] {
        if let Some(path) = which(candidate) {
            shells.push(path.to_string_lossy().into_owned());
        }
    }
    if let Some(comspec) = std::env::var("COMSPEC")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        shells.push(comspec);
    }
    shells
}

#[cfg(not(windows))]
fn platform_shells() -> Vec<String> {
    let listed: Vec<String> = std::fs::read_to_string("/etc/shells").map_or_else(
        |_| Vec::new(),
        |text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect()
        },
    );
    if listed.is_empty() {
        vec![
            "/bin/bash".to_owned(),
            "/bin/zsh".to_owned(),
            "/bin/sh".to_owned(),
        ]
    } else {
        listed
    }
}

/// Resolve a configured path or a bare program name on `PATH`.
fn executable(file: &str) -> Option<PathBuf> {
    if file.is_empty() {
        return None;
    }
    let path = Path::new(file);
    if path.is_absolute() || path.components().count() > 1 {
        return runnable(path).then(|| path.to_path_buf());
    }
    which(file)
}

fn runnable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn which(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        for candidate in candidates(&directory, program) {
            if runnable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(windows)]
    {
        let extensions = std::env::var("PATHEXT")
            .ok()
            .map(|value| {
                value
                    .split(';')
                    .filter_map(|extension| {
                        let extension = extension.trim().trim_start_matches('.');
                        (!extension.is_empty()).then(|| extension.to_ascii_lowercase())
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|extensions: &Vec<String>| !extensions.is_empty())
            .unwrap_or_else(|| {
                vec![
                    "exe".to_owned(),
                    "cmd".to_owned(),
                    "bat".to_owned(),
                    "com".to_owned(),
                ]
            });
        for extension in extensions {
            candidates.push(directory.join(format!("{program}.{extension}")));
        }
    }
    candidates.push(directory.join(program));
    candidates
}

/// Upstream treats fish and nushell as terminal-only.
fn compatible(name: &str) -> bool {
    !matches!(name, "fish" | "nu")
}

/// `GET /api/skill` — discovered skills, including their bodies.
pub async fn skills(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let config = require_config(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let skills: Vec<Value> = config
        .skills()
        .iter()
        .map(|skill| {
            let mut map = Map::new();
            map.insert("id".to_owned(), json!(skill.id));
            map.insert("name".to_owned(), json!(skill.name));
            if let Some(description) = &skill.description {
                map.insert("description".to_owned(), json!(description));
            }
            if let Some(autoinvoke) = skill.autoinvoke {
                map.insert("autoinvoke".to_owned(), json!(autoinvoke));
            }
            map.insert("path".to_owned(), json!(skill.path.to_string_lossy()));
            map.insert("content".to_owned(), json!(skill.content));
            Value::Object(map)
        })
        .collect();
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(skills),
    )))
}

/// `GET /api/command` — discovered markdown slash commands.
pub async fn commands(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let config = require_config(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let mut commands: Vec<Value> = config
        .commands()
        .iter()
        .map(|command| {
            let mut map = Map::new();
            map.insert("name".to_owned(), json!(command.name));
            if let Some(description) = &command.description {
                map.insert("description".to_owned(), json!(description));
            }
            if let Some(agent) = &command.agent {
                map.insert("agent".to_owned(), json!(agent));
            }
            if let Some(model) = &command.model {
                map.insert("model".to_owned(), json!(model));
            }
            if let Some(subtask) = command.subtask {
                map.insert("subtask".to_owned(), json!(subtask));
            }
            map.insert("template".to_owned(), json!(command.template));
            map.insert("path".to_owned(), json!(command.path.to_string_lossy()));
            Value::Object(map)
        })
        .collect();
    // Builtin alias of the upstream `/compact` command. It uses only the
    // documented `Command.Info` fields (`name`, `description`); a markdown
    // command named `compress` overrides it in `POST /command`.
    if !config
        .commands()
        .iter()
        .any(|command| command.name.eq_ignore_ascii_case("compress"))
    {
        commands.push(json!({
            "name": "compress",
            "description": "Compress the session to reduce context size",
        }));
    }
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(commands),
    )))
}

/// `GET /api/reference` — configured references are not materialized yet.
///
/// An empty/absent `references` map is truthfully an empty list. If the user
/// configured references, returning `[]` would hide them, so the route fails
/// closed until the registry can resolve their paths.
pub async fn references(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let config = require_config(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let configured = config
        .merged()
        .get("references")
        .and_then(Value::as_object)
        .is_some_and(|references| !references.is_empty());
    if configured {
        return Err(ApiError::service_unavailable("reference registry not loaded").into());
    }
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(Vec::new()),
    )))
}

/// `GET /api/agent`
pub async fn agents(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let config = require_config(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(config.agents()),
    )))
}

/// `GET /api/agent/:id`
pub async fn agent(
    AxumPath(id): AxumPath<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let config = require_config(&state)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    let Some(agent) = config.agent(&id) else {
        return Err(
            ApiError::not_found("AgentNotFoundError", format!("Agent not found: {id}"))
                .with("agentID", json!(id))
                .into(),
        );
    };
    Ok(Json(LocationEnvelope::new(resolved.reference(), agent)))
}
