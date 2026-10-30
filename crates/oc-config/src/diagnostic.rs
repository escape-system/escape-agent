//! Diagnostics collected while discovering and normalizing configuration.
//!
//! A broken document never fails a load: it is skipped and the reason is
//! recorded here (docs/14, "tolerant errors as diagnostics").

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// The value could not be parsed or had the wrong shape.
    Invalid,
    /// The key exists in an older format and was dropped or translated.
    Unsupported,
    /// Legacy and native forms disagreed; the native value was kept.
    Conflict,
    /// A referenced file could not be read.
    Io,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// File or virtual source that produced the diagnostic; absent for inline content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// JSON-style path such as `$.agents.build.model`.
    pub path: String,
    pub kind: DiagnosticKind,
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(
        source: Option<&str>,
        kind: DiagnosticKind,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source: source.map(str::to_owned),
            path: path.into(),
            kind,
            message: message.into(),
        }
    }
}
