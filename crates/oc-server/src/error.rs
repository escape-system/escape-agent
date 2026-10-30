//! `IntoResponse` adapter for [`oc_types::ApiError`].
//!
//! The orphan rule prevents implementing `axum::response::IntoResponse` for a
//! type defined in `oc-types`, so the server wraps it here.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oc_types::ApiError;

/// Newtype carrying an API error into an HTTP response.
#[derive(Debug)]
pub struct ErrorResponse(pub ApiError);

impl From<ApiError> for ErrorResponse {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

impl From<oc_store::StoreError> for ErrorResponse {
    fn from(error: oc_store::StoreError) -> Self {
        use oc_store::StoreError;
        let mapped = match &error {
            StoreError::Conflict(message) => ApiError::conflict(message.clone()),
            StoreError::NotFound(message) => ApiError::not_found("NotFoundError", message.clone()),
            StoreError::InvalidCursor(message) | StoreError::Invalid(message) => {
                ApiError::new(400, "BadRequestError", message.clone())
            }
            StoreError::Sqlite(_)
            | StoreError::Io(_)
            | StoreError::Json(_)
            | StoreError::Corrupt(_)
            | StoreError::Closed
            | StoreError::Task(_)
            | StoreError::MigrationChecksum(_) => {
                tracing::error!(error = %error, "store operation failed");
                ApiError::internal("Internal error")
            }
        };
        Self(mapped)
    }
}

impl From<oc_memory::MemoryError> for ErrorResponse {
    fn from(error: oc_memory::MemoryError) -> Self {
        use oc_memory::MemoryError;
        let mapped = match error {
            MemoryError::Store(error) => return Self::from(error),
            MemoryError::Secret { rule } => ApiError::bad_request(format!(
                "Memory rejected: the text contains what looks like a {rule}"
            )),
            MemoryError::Invalid(message) => ApiError::bad_request(message),
            MemoryError::TextTooLong { chars, cap } => ApiError::bad_request(format!(
                "memory text is {chars} characters; the cap is {cap} characters"
            )),
            MemoryError::ModelJson(message) => ApiError::new(
                502,
                "ProviderError",
                format!("Memory model JSON: {message}"),
            ),
            MemoryError::Model(message) => ApiError::new(
                502,
                "ProviderError",
                format!("Memory model call: {message}"),
            ),
        };
        Self(mapped)
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.body())).into_response()
    }
}

/// Convenience extension so handlers can write `error.respond()`.
pub trait Respond {
    fn respond(self) -> Response;
}

impl Respond for ApiError {
    fn respond(self) -> Response {
        ErrorResponse(self).into_response()
    }
}
