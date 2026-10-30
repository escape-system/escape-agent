//! Integration and credential routes.
//!
//! Upstream keeps a registry of integrations that plugins and the MCP manager
//! populate, plus a credential table. This build has no plugin host attached, so
//! the registry is derived from what the process really knows:
//!
//! - every provider the catalog knows (skipping `Disabled` ones), merged with
//!   the configured `providers` map, which overrides the name and environment
//!   variables; each row has the `key` method upstream registers ("Manually
//!   enter API Key") and an `env` method when the effective env list is not
//!   empty;
//! - every remote MCP server whose OAuth is not disabled, with one `oauth`
//!   method (id and integration id follow upstream: `mcp_` plus a hash of
//!   `name\0url`);
//! - integrations that exist only because a stored credential references them,
//!   listed without methods so no capability is invented.
//!
//! Connections are the credentials stored for an integration (newest first)
//! plus the environment variables of its `env` methods that are actually set.
//!
//! Credentials are persisted through `oc-store`; optional wire fields are
//! omitted, never sent as `null`. Updating, activating or removing an unknown
//! credential is the upstream no-op and answers `204`, while the extra
//! `GET /api/credential/{id}` (not in the `OpenAPI` document) answers `404`
//! `NotFoundError` for a missing row.
//!
//! OAuth, external and command authorization need engines or method
//! registrations this build does not attach: their routes exist and validate
//! their input, then answer the documented `400 InvalidRequestError` for a
//! method that is not registered and `501 NotImplementedError` for a method that
//! is registered but has no engine behind it. Attempt status routes answer the
//! documented `404 IntegrationAttemptNotFoundError` because no attempt can
//! exist. Well-known discovery answers `503` because discovery is not
//! configured.

use std::collections::{BTreeSet, HashSet};

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use oc_llm::Activation;
use oc_mcp::{McpServerConfig, OAuthSetting};
use oc_store::{CredentialPatch, NewCredential, StoreError};
use oc_types::{ApiError, DataEnvelope, LocationEnvelope};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::data::location;
use crate::data::session::LocationQuery;
use crate::data::{no_content, query_error, require_store, schema_error};
use crate::error::ErrorResponse;

/// Credential and integration routes, merged into the protected router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/credential",
            get(credential_list).post(credential_create),
        )
        .route(
            "/api/credential/{id}",
            get(credential_get)
                .patch(credential_update)
                .delete(credential_remove),
        )
        .route("/api/credential/{id}/activate", post(credential_activate))
        .route("/api/integration", get(integration_list))
        .route("/api/integration/{id}", get(integration_get))
        .route("/api/integration/{id}/connect/key", post(connect_key))
        .route(
            "/api/integration/{id}/connect/external",
            post(connect_external),
        )
        .route("/api/integration/{id}/connect/oauth", post(connect_oauth))
        .route(
            "/api/integration/{id}/connect/oauth/{attempt_id}",
            get(oauth_status).delete(oauth_cancel),
        )
        .route(
            "/api/integration/{id}/connect/oauth/{attempt_id}/complete",
            post(oauth_complete),
        )
        .route(
            "/api/integration/{id}/connect/command",
            post(command_connect),
        )
        .route(
            "/api/integration/{id}/connect/command/{attempt_id}",
            get(command_status).delete(command_cancel),
        )
        .route(
            "/api/experimental/integration/wellknown",
            get(wellknown_status).post(wellknown_add),
        )
}

/// `Credential.Key` — only the documented members are accepted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyCredential {
    #[serde(rename = "type")]
    _kind: String,
    key: String,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

/// `Credential.OAuth`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthCredential {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(rename = "methodID")]
    method_id: String,
    refresh: String,
    access: String,
    expires: u64,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

/// `Credential.External`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalCredential {
    #[serde(rename = "type")]
    _kind: String,
    #[serde(rename = "methodID")]
    method_id: String,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
}

/// Decode one `Credential.Value` strictly.
///
/// The tagged union is dispatched by hand so unknown members are rejected
/// (serde ignores `deny_unknown_fields` on internally tagged enums) and every
/// optional member is omitted rather than stored as `null`.
fn parse_credential_value(value: Value) -> Result<Value, ApiError> {
    let Some(tag) = value.get("type").and_then(Value::as_str).map(str::to_owned) else {
        return Err(invalid_value_message(
            "credential value must be an object with a string type",
        ));
    };
    match tag.as_str() {
        "key" => {
            let parsed: KeyCredential =
                serde_json::from_value(value).map_err(|error| invalid_value(&error))?;
            Ok(with_metadata(
                json!({ "type": "key", "key": parsed.key }),
                parsed.metadata,
            ))
        }
        "oauth" => {
            let parsed: OAuthCredential =
                serde_json::from_value(value).map_err(|error| invalid_value(&error))?;
            Ok(with_metadata(
                json!({
                    "type": "oauth",
                    "methodID": parsed.method_id,
                    "refresh": parsed.refresh,
                    "access": parsed.access,
                    "expires": parsed.expires,
                }),
                parsed.metadata,
            ))
        }
        "external" => {
            let parsed: ExternalCredential =
                serde_json::from_value(value).map_err(|error| invalid_value(&error))?;
            Ok(with_metadata(
                json!({ "type": "external", "methodID": parsed.method_id }),
                parsed.metadata,
            ))
        }
        other => Err(invalid_value_message(&format!(
            "credential type must be oauth, key or external: {other}"
        ))),
    }
}

/// A rejected `Credential.Value` body member.
fn invalid_value(error: &serde_json::Error) -> ApiError {
    invalid_value_message(&format!("invalid credential value: {error}"))
}

fn invalid_value_message(message: &str) -> ApiError {
    ApiError::new(400, "SchemaError", message)
}

/// Attach the optional metadata object; an absent one stays absent.
fn with_metadata(mut value: Value, metadata: Option<Map<String, Value>>) -> Value {
    if let (Some(metadata), Some(object)) = (metadata, value.as_object_mut()) {
        object.insert("metadata".to_owned(), Value::Object(metadata));
    }
    value
}

/// `POST /api/credential`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateCredentialBody {
    id: Option<String>,
    #[serde(rename = "integrationID")]
    integration_id: String,
    label: Option<String>,
    value: Value,
    activate: Option<bool>,
}

/// `PATCH /api/credential/{id}`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCredentialBody {
    label: String,
}

/// `POST /api/integration/{id}/connect/key`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyConnectBody {
    key: String,
    #[serde(default)]
    answer: Option<Map<String, Value>>,
    label: Option<String>,
}

/// `POST /api/integration/{id}/connect/external` and `/connect/oauth`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MethodConnectBody {
    #[serde(rename = "methodID")]
    method_id: String,
    #[serde(default)]
    answer: Option<Map<String, Value>>,
    label: Option<String>,
}

/// `POST /api/integration/{id}/connect/command`
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandConnectBody {
    #[serde(rename = "methodID")]
    method_id: String,
    /// Accepted by the schema; an attempt cannot be started, so it is unused.
    #[serde(rename = "label")]
    _label: Option<String>,
}

/// `POST /api/experimental/integration/wellknown`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WellKnownBody {
    url: String,
}

/// One registry entry, before credentials are attached as connections.
#[derive(Debug)]
struct IntegrationRow {
    id: String,
    name: String,
    metadata: Option<Value>,
    methods: Vec<Value>,
}

/// `GET /api/credential`
pub async fn credential_list(
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Vec<Value>>>, ErrorResponse> {
    let store = require_store(&state)?;
    Ok(Json(DataEnvelope::new(store.list_credentials().await?)))
}

/// `GET /api/credential/{id}` — not part of the `OpenAPI` document; the entry
/// shape is the documented `Credential.Entry`.
pub async fn credential_get(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let store = require_store(&state)?;
    let entry = store
        .get_credential(&id)
        .await?
        .ok_or_else(|| credential_not_found(&id))?;
    Ok(Json(DataEnvelope::new(entry)))
}

/// `POST /api/credential`
pub async fn credential_create(
    State(state): State<AppState>,
    body: Result<Json<CreateCredentialBody>, JsonRejection>,
) -> Result<Json<DataEnvelope<Value>>, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let value = parse_credential_value(body.value)?;
    let store = require_store(&state)?;
    if let Some(id) = body.id.as_deref()
        && store.get_credential(id).await?.is_some()
    {
        return Err(ErrorResponse::from(
            ApiError::conflict(format!("Credential already exists: {id}"))
                .with("resource", json!(id)),
        ));
    }
    let entry = store
        .create_credential(NewCredential {
            id: body.id,
            integration_id: body.integration_id,
            label: body.label,
            value,
            active: body.activate,
        })
        .await?;
    Ok(Json(DataEnvelope::new(entry)))
}

/// `PATCH /api/credential/{id}` — an unknown credential is the upstream no-op.
pub async fn credential_update(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<UpdateCredentialBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    store
        .update_credential(
            &id,
            CredentialPatch {
                label: Some(body.label),
                ..CredentialPatch::default()
            },
        )
        .await?;
    Ok(no_content())
}

/// `DELETE /api/credential/{id}` — an unknown credential is the upstream no-op.
pub async fn credential_remove(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    store.delete_credential(&id).await?;
    Ok(no_content())
}

/// `POST /api/credential/{id}/activate` — an unknown credential is the upstream
/// no-op, so `404` is reserved for the documented errors only.
pub async fn credential_activate(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Response, ErrorResponse> {
    let store = require_store(&state)?;
    match store.activate_credential(&id).await {
        Ok(_) | Err(StoreError::NotFound(_)) => Ok(no_content()),
        Err(error) => Err(ErrorResponse::from(error)),
    }
}

/// `GET /api/integration`
pub async fn integration_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let (rows, credentials) = snapshot(&state).await?;
    let data: Vec<Value> = rows
        .iter()
        .map(|row| integration_info(row, &credentials))
        .collect();
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        Value::Array(data),
    )))
}

/// `GET /api/integration/{id}`
pub async fn integration_get(
    Path(id): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    params: Result<Query<LocationQuery>, QueryRejection>,
) -> Result<Json<LocationEnvelope<Value>>, ErrorResponse> {
    let Query(params) = params.map_err(|error| ErrorResponse::from(query_error(error)))?;
    let (rows, credentials) = snapshot(&state).await?;
    let row = find_row(&rows, &id)?;
    let resolved = location::resolve(&state, &headers, None, params.directory.as_deref());
    Ok(Json(LocationEnvelope::new(
        resolved.reference(),
        integration_info(row, &credentials),
    )))
}

/// `POST /api/integration/{id}/connect/key`
pub async fn connect_key(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<KeyConnectBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let credentials = store.list_credentials().await?;
    let rows = derive_rows(&state, &credentials);
    let row = find_row(&rows, &id)?;
    if !has_method(row, "key", None) {
        return Err(ErrorResponse::from(invalid_request(
            "integration_authorization",
            format!("Key method not found: {id}"),
        )));
    }
    // The registered key method has no form, so an answer has nowhere to go.
    if body
        .answer
        .as_ref()
        .is_some_and(|answer| !answer.is_empty())
    {
        return Err(ErrorResponse::from(invalid_request(
            "integration_authorization",
            "Key method does not accept a form answer",
        )));
    }
    let label = credential_label(&row.name, body.label, &credentials, &id);
    store
        .create_credential(NewCredential {
            id: None,
            integration_id: id,
            label: Some(label),
            value: json!({ "type": "key", "key": body.key }),
            active: None,
        })
        .await?;
    Ok(no_content())
}

/// `POST /api/integration/{id}/connect/external`
pub async fn connect_external(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<MethodConnectBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let store = require_store(&state)?;
    let credentials = store.list_credentials().await?;
    let rows = derive_rows(&state, &credentials);
    let row = find_row(&rows, &id)?;
    if !has_method(row, "external", Some(&body.method_id)) {
        return Err(ErrorResponse::from(invalid_request(
            "integration_authorization",
            format!("External method not found: {}", body.method_id),
        )));
    }
    if body
        .answer
        .as_ref()
        .is_some_and(|answer| !answer.is_empty())
    {
        return Err(ErrorResponse::from(invalid_request(
            "integration_authorization",
            "External method does not accept a form answer",
        )));
    }
    let label = credential_label(&row.name, body.label, &credentials, &id);
    store
        .create_credential(NewCredential {
            id: None,
            integration_id: id,
            label: Some(label),
            value: json!({ "type": "external", "methodID": body.method_id }),
            active: None,
        })
        .await?;
    Ok(no_content())
}

/// `POST /api/integration/{id}/connect/oauth`
///
/// The method is validated against the registry, but no OAuth authorization
/// engine is attached, so no attempt can be started.
pub async fn connect_oauth(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<MethodConnectBody>, JsonRejection>,
) -> ErrorResponse {
    let body = match body {
        Ok(Json(body)) => body,
        Err(error) => return schema_error(error).into(),
    };
    let (rows, _credentials) = match snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(error) => return error,
    };
    let row = match find_row(&rows, &id) {
        Ok(row) => row,
        Err(error) => return error,
    };
    if !has_method(row, "oauth", Some(&body.method_id)) {
        return invalid_request(
            "integration_authorization",
            format!("OAuth method not found: {id}/{}", body.method_id),
        )
        .into();
    }
    not_implemented("OAuth authorization is not available").into()
}

/// `GET /api/integration/{id}/connect/oauth/{attemptID}`
pub async fn oauth_status(Path((id, attempt_id)): Path<(String, String)>) -> ErrorResponse {
    attempt_not_found(&id, &attempt_id).into()
}

/// `DELETE /api/integration/{id}/connect/oauth/{attemptID}` — cancelling an
/// attempt that does not exist is a no-op upstream.
pub async fn oauth_cancel(Path((_id, _attempt_id)): Path<(String, String)>) -> Response {
    no_content()
}

/// `POST /api/integration/{id}/connect/oauth/{attemptID}/complete`
pub async fn oauth_complete(Path((id, attempt_id)): Path<(String, String)>) -> ErrorResponse {
    attempt_not_found(&id, &attempt_id).into()
}

/// `POST /api/integration/{id}/connect/command`
pub async fn command_connect(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Result<Json<CommandConnectBody>, JsonRejection>,
) -> ErrorResponse {
    let body = match body {
        Ok(Json(body)) => body,
        Err(error) => return schema_error(error).into(),
    };
    let (rows, _credentials) = match snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(error) => return error,
    };
    let row = match find_row(&rows, &id) {
        Ok(row) => row,
        Err(error) => return error,
    };
    if !has_method(row, "command", Some(&body.method_id)) {
        return invalid_request(
            "integration_authorization",
            format!("Command method not found: {id}/{}", body.method_id),
        )
        .into();
    }
    not_implemented("command authorization is not available").into()
}

/// `GET /api/integration/{id}/connect/command/{attemptID}`
pub async fn command_status(Path((id, attempt_id)): Path<(String, String)>) -> ErrorResponse {
    attempt_not_found(&id, &attempt_id).into()
}

/// `DELETE /api/integration/{id}/connect/command/{attemptID}` — cancelling an
/// attempt that does not exist is a no-op upstream.
pub async fn command_cancel(Path((_id, _attempt_id)): Path<(String, String)>) -> Response {
    no_content()
}

/// `GET /api/experimental/integration/wellknown`
///
/// Upstream only exposes `POST` on this path; the read form reports the same
/// missing configuration instead of pretending discovery is available.
pub async fn wellknown_status() -> ErrorResponse {
    ApiError::service_unavailable("well-known integration discovery is not configured").into()
}

/// `POST /api/experimental/integration/wellknown`
///
/// The body is validated, then discovery is refused: no well-known source
/// engine is configured.
pub async fn wellknown_add(
    body: Result<Json<WellKnownBody>, JsonRejection>,
) -> Result<Response, ErrorResponse> {
    let Json(body) = body.map_err(|error| ErrorResponse::from(schema_error(error)))?;
    let parsed = url::Url::parse(&body.url).map_err(|error| {
        ErrorResponse::from(invalid_request(
            "well_known_discovery",
            format!("invalid integration URL: {error}"),
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ErrorResponse::from(invalid_request(
            "well_known_discovery",
            "integration URL must use http or https",
        )));
    }
    Err(ErrorResponse::from(ApiError::service_unavailable(
        "well-known integration discovery is not configured",
    )))
}

/// The credential store's view plus the derived registry.
///
/// A missing store means no credential can be read at all, so the registry is
/// served without connections instead of failing the read-only routes.
async fn snapshot(state: &AppState) -> Result<(Vec<IntegrationRow>, Vec<Value>), ErrorResponse> {
    let credentials = match state.store() {
        Some(store) => store.list_credentials().await?,
        None => Vec::new(),
    };
    let rows = derive_rows(state, &credentials);
    Ok((rows, credentials))
}

/// Every integration this process knows about, sorted by name like upstream.
fn derive_rows(state: &AppState, credentials: &[Value]) -> Vec<IntegrationRow> {
    let mut rows = provider_rows(state);
    rows.extend(mcp_rows(state));
    let mut seen: HashSet<String> = rows.iter().map(|row| row.id.clone()).collect();
    for credential in credentials {
        let Some(integration_id) = credential.get("integrationID").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(integration_id.to_owned()) {
            rows.push(IntegrationRow {
                id: integration_id.to_owned(),
                name: integration_id.to_owned(),
                metadata: None,
                methods: Vec::new(),
            });
        }
    }
    rows.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    rows
}

/// Every known model provider: catalog providers (never `Disabled`) merged with
/// configured `providers`, which override the catalog name and `env` list.
///
/// Each row carries the `key` method upstream registers so the UI can connect a
/// key for any provider, plus an `env` method when the effective env list is not
/// empty.
fn provider_rows(state: &AppState) -> Vec<IntegrationRow> {
    let configured = state
        .config()
        .and_then(|config| config.merged().get("providers").and_then(Value::as_object));

    let mut rows: Vec<IntegrationRow> = Vec::new();
    if let Some(catalog) = state.catalog() {
        for provider in catalog.providers() {
            if provider.activation == Activation::Disabled {
                continue;
            }
            let env: Vec<&str> = provider.env.iter().map(String::as_str).collect();
            let mut row = IntegrationRow {
                id: provider.id.clone(),
                name: non_empty(&provider.name)
                    .unwrap_or(provider.id.as_str())
                    .to_owned(),
                metadata: None,
                methods: provider_methods(&env),
            };
            if let Some(config) = configured.and_then(|providers| providers.get(&provider.id)) {
                if config_disabled(config) {
                    continue;
                }
                apply_provider_config(&mut row, config);
            }
            rows.push(row);
        }
    }
    if let Some(providers) = configured {
        for (id, config) in providers {
            if config_disabled(config) || rows.iter().any(|row| row.id == *id) {
                continue;
            }
            let mut row = IntegrationRow {
                id: id.clone(),
                name: id.clone(),
                metadata: None,
                methods: provider_methods(&[]),
            };
            apply_provider_config(&mut row, config);
            rows.push(row);
        }
    }
    rows
}

/// The `key` method every provider exposes, plus `env` when names are declared.
fn provider_methods(env: &[&str]) -> Vec<Value> {
    let mut methods = vec![json!({ "type": "key", "label": "Manually enter API Key" })];
    if !env.is_empty() {
        methods.push(json!({ "type": "env", "names": env }));
    }
    methods
}

/// Configured name and `env` list override the catalog row.
fn apply_provider_config(row: &mut IntegrationRow, config: &Value) {
    if let Some(name) = config
        .get("name")
        .and_then(Value::as_str)
        .and_then(non_empty)
    {
        name.clone_into(&mut row.name);
    }
    if let Some(env) = config.get("env") {
        let names: Vec<&str> = env
            .as_array()
            .map(|names| names.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        row.methods = provider_methods(&names);
    }
}

/// Whether the provider config turns the provider off, matching the catalog
/// overlay precedence: `enabled`, then `disabled`, then `activation`.
fn config_disabled(config: &Value) -> bool {
    if let Some(enabled) = config.get("enabled").and_then(Value::as_bool) {
        return !enabled;
    }
    if config.get("disabled").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    config.get("activation").and_then(Value::as_str) == Some("disabled")
}

fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// Remote MCP servers that keep OAuth enabled, keyed exactly like upstream.
fn mcp_rows(state: &AppState) -> Vec<IntegrationRow> {
    let Some(manager) = state.mcp() else {
        return Vec::new();
    };
    manager
        .servers()
        .into_iter()
        .filter_map(|(name, config)| {
            let McpServerConfig::Remote(remote) = config else {
                return None;
            };
            if matches!(remote.oauth.as_ref(), Some(OAuthSetting::Disabled(_))) {
                return None;
            }
            let id = mcp_integration_id(&name, &remote.url);
            let method = json!({ "id": id, "type": "oauth", "label": name });
            Some(IntegrationRow {
                id,
                name,
                metadata: Some(json!({ "source": "mcp" })),
                methods: vec![method],
            })
        })
        .collect()
}

/// `mcp_` plus the first 16 hex digits of a hash of `name\0url`.
///
/// Upstream uses SHA-1; this build already depends on SHA-2, so the digest
/// differs but the identity rule (name and URL, never URL alone) is the same.
fn mcp_integration_id(name: &str, url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0_u8]);
    hasher.update(url.as_bytes());
    let digest = hasher.finalize();
    format!("mcp_{}", hex::encode(&digest[..8]))
}

/// Build the wire `Integration.Info` for one row.
fn integration_info(row: &IntegrationRow, credentials: &[Value]) -> Value {
    let mut object = Map::new();
    object.insert("id".to_owned(), json!(row.id));
    object.insert("name".to_owned(), json!(row.name));
    if let Some(metadata) = &row.metadata {
        object.insert("metadata".to_owned(), metadata.clone());
    }
    object.insert("methods".to_owned(), Value::Array(row.methods.clone()));
    object.insert(
        "connections".to_owned(),
        Value::Array(connections(row, credentials)),
    );
    Value::Object(object)
}

/// Stored credentials of the integration (newest first) plus set env names.
fn connections(row: &IntegrationRow, credentials: &[Value]) -> Vec<Value> {
    let mut items: Vec<Value> = credentials
        .iter()
        .filter(|credential| {
            credential.get("integrationID").and_then(Value::as_str) == Some(row.id.as_str())
        })
        .filter_map(credential_connection)
        .collect();
    items.reverse();
    for method in &row.methods {
        if method.get("type").and_then(Value::as_str) != Some("env") {
            continue;
        }
        let Some(names) = method.get("names").and_then(Value::as_array) else {
            continue;
        };
        for name in names.iter().filter_map(Value::as_str) {
            if std::env::var(name).is_ok_and(|value| !value.is_empty()) {
                items.push(json!({ "type": "env", "name": name }));
            }
        }
    }
    items
}

/// One `Connection.CredentialInfo`, or `None` for a row that is not a valid
/// `Credential.Value` (the connection is then invisible rather than invented).
fn credential_connection(credential: &Value) -> Option<Value> {
    let method = credential.get("value")?.get("type")?.as_str()?;
    if !matches!(method, "key" | "oauth" | "external") {
        return None;
    }
    Some(json!({
        "type": "credential",
        "id": credential.get("id")?.as_str()?,
        "label": credential.get("label")?.as_str()?,
        "method": method,
    }))
}

/// One row by id, or the documented `IntegrationNotFoundError`.
fn find_row<'a>(rows: &'a [IntegrationRow], id: &str) -> Result<&'a IntegrationRow, ErrorResponse> {
    rows.iter()
        .find(|row| row.id == id)
        .ok_or_else(|| ErrorResponse::from(integration_not_found(id)))
}

/// Whether the row registers a method of `kind` (optionally with `method_id`).
fn has_method(row: &IntegrationRow, kind: &str, method_id: Option<&str>) -> bool {
    row.methods.iter().any(|method| {
        method.get("type").and_then(Value::as_str) == Some(kind)
            && method_id
                .is_none_or(|wanted| method.get("id").and_then(Value::as_str) == Some(wanted))
    })
}

/// An explicit label wins; otherwise the upstream `<name>`, `<name> 2`, ... rule.
fn credential_label(
    name: &str,
    requested: Option<String>,
    credentials: &[Value],
    integration_id: &str,
) -> String {
    if let Some(label) = requested {
        return label;
    }
    let taken: BTreeSet<&str> = credentials
        .iter()
        .filter(|credential| {
            credential.get("integrationID").and_then(Value::as_str) == Some(integration_id)
        })
        .filter_map(|credential| credential.get("label").and_then(Value::as_str))
        .collect();
    for index in 0..=taken.len() {
        let candidate = if index == 0 {
            name.to_owned()
        } else {
            format!("{name} {}", index + 1)
        };
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    name.to_owned()
}

fn credential_not_found(id: &str) -> ErrorResponse {
    ApiError::not_found("NotFoundError", format!("Credential not found: {id}")).into()
}

fn integration_not_found(id: &str) -> ApiError {
    ApiError::not_found(
        "IntegrationNotFoundError",
        format!("Integration not found: {id}"),
    )
    .with("integrationID", json!(id))
}

fn attempt_not_found(integration_id: &str, attempt_id: &str) -> ApiError {
    ApiError::not_found(
        "IntegrationAttemptNotFoundError",
        format!("Integration attempt not found: {attempt_id}"),
    )
    .with("integrationID", json!(integration_id))
    .with("attemptID", json!(attempt_id))
}

fn invalid_request(kind: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(400, "InvalidRequestError", message).with("kind", json!(kind))
}

fn not_implemented(message: &str) -> ApiError {
    ApiError::new(501, "NotImplementedError", message)
}
