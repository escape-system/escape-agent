//! Diagnostic sink shared by every normalization step.

use crate::diagnostic::{Diagnostic, DiagnosticKind};

pub(crate) struct Ctx<'a> {
    pub source: Option<&'a str>,
    pub diagnostics: Vec<Diagnostic>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(source: Option<&'a str>) -> Self {
        Self {
            source,
            diagnostics: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, kind: DiagnosticKind, path: String, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::new(self.source, kind, path, message));
    }

    pub(super) fn invalid(&mut self, path: &str) {
        self.push(
            DiagnosticKind::Invalid,
            path.to_owned(),
            "skipped malformed recognized value",
        );
    }

    pub(super) fn unsupported(&mut self, path: &str, message: &str) {
        self.push(DiagnosticKind::Unsupported, path.to_owned(), message);
    }

    pub(super) fn conflict(&mut self, path: &str) {
        self.push(
            DiagnosticKind::Conflict,
            path.to_owned(),
            "retained native value over legacy value",
        );
    }
}
