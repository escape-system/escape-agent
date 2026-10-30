//! Pagination cursors, ordering and shared encoding helpers.
//!
//! Message cursors are base64url JSON `{id, order, direction}`; session cursors
//! additionally carry the resolved anchor time and the filters they were built
//! with. Both are opaque to clients.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Keyset cursors for one page, in the requested order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Cursor that walks towards the start of the list.
    pub previous: Option<String>,
    /// Cursor that walks towards the end of the list.
    pub next: Option<String>,
}

/// Sort order for keyset pagination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    /// Oldest / lowest key first.
    Asc,
    /// Newest / highest key first.
    #[default]
    Desc,
}

impl Order {
    pub(crate) fn flipped(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }

    pub(crate) fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// Direction of the page requested relative to its anchor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Continue away from the start of the list.
    #[default]
    Next,
    /// Walk back towards the start of the list.
    Previous,
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String, StoreError> {
    let bytes = serde_json::to_vec(value)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn decode<T: DeserializeOwned>(raw: &str) -> Result<T, StoreError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| StoreError::InvalidCursor("cursor is not base64url encoded".to_owned()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| StoreError::InvalidCursor(format!("cursor json is invalid: {error}")))
}
