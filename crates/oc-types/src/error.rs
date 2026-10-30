//! Effect-compatible tagged error bodies.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// An error serialized as `{"_tag": "...", "message": "...", ...}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    status: u16,
    tag: String,
    message: String,
    extra: BTreeMap<String, Value>,
}

impl ApiError {
    #[must_use]
    pub fn new(status: u16, tag: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            tag: tag.into(),
            message: message.into(),
            extra: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn unauthorized() -> Self {
        Self::new(401, "UnauthorizedError", "Authentication required")
    }

    #[must_use]
    pub fn not_found(tag: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(404, tag, message)
    }

    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "BadRequestError", message)
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "ConflictError", message)
    }

    #[must_use]
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(503, "ServiceUnavailableError", message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "InternalError", message)
    }

    /// Attach an entity-specific field to the error body.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    #[must_use]
    pub fn body(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("_tag".to_owned(), json!(self.tag));
        map.insert("message".to_owned(), json!(self.message));
        for (key, value) in &self.extra {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.tag, self.message)
    }
}

impl std::error::Error for ApiError {}
