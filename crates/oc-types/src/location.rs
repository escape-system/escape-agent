//! Location and project references.

use serde::{Deserialize, Serialize};

use crate::id::ProjectId;

/// Project identity as exposed on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRef {
    pub id: ProjectId,
    pub directory: String,
    pub canonical: String,
}

/// Full location reference used by `Location.response(...)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocationRef {
    pub directory: String,
    pub project: ProjectRef,
}

/// Location as carried by events: directory only (upstream `PublicRef`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventLocation {
    pub directory: String,
}

/// `{ location, data }` envelope. The generated client does not unwrap it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocationEnvelope<T> {
    pub location: LocationRef,
    pub data: T,
}

impl<T> LocationEnvelope<T> {
    #[must_use]
    pub fn new(location: LocationRef, data: T) -> Self {
        Self { location, data }
    }
}

/// `{ data }` envelope. The generated client unwraps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataEnvelope<T> {
    pub data: T,
}

impl<T> DataEnvelope<T> {
    #[must_use]
    pub fn new(data: T) -> Self {
        Self { data }
    }
}
