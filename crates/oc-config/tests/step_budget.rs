//! `step_budget.*` must survive normalization.
//!
//! The runtime reads its extension policy from the merged config
//! (`crates/oc-runtime/src/turn/budget.rs` `BudgetPolicy::from_merged`, called
//! from `turn/run.rs` and `observer/repair_worker.rs`). A key that is listed in
//! `normalize/keys.rs` `HANDLED` but never copied is dropped by normalization
//! and then skipped by the unknown-key loop, so it could never reach the
//! runtime. These tests assert the merged document — the exact value
//! `ConfigService::merged()` returns and the runtime passes to
//! `from_merged` — rather than the private normalizer.
//!
//! Defaults are deliberately *not* materialized: the documented fallbacks live
//! in `BudgetPolicy::default()` and an absent key must keep using them, so a
//! later default change is not frozen into previously written configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_config::{ConfigService, DiagnosticKind, LoadOptions};
use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("temp dir"),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn project(&self) -> PathBuf {
        let path = self.path("project");
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }

    fn options(&self, directory: &Path) -> LoadOptions {
        let mut env = HashMap::new();
        env.insert(
            "HOME".to_owned(),
            self.path("home").to_string_lossy().into_owned(),
        );
        env.insert(
            "XDG_CONFIG_HOME".to_owned(),
            self.path("xdg").to_string_lossy().into_owned(),
        );
        env.insert(
            "ProgramData".to_owned(),
            self.path("programdata").to_string_lossy().into_owned(),
        );
        env.insert(
            "OPENCODE_CONFIG".to_owned(),
            self.path("missing-explicit.json")
                .to_string_lossy()
                .into_owned(),
        );
        // Layer a non-empty placeholder so a runner that exports the real
        // variable cannot inject a document into these fixtures.
        env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
        LoadOptions {
            directory: directory.to_path_buf(),
            env,
        }
    }

    /// Load with an inline `OPENCODE_CONFIG_CONTENT` document.
    fn load_inline(&self, document: String) -> ConfigService {
        let project = self.project();
        let mut options = self.options(&project);
        options
            .env
            .insert("OPENCODE_CONFIG_CONTENT".to_owned(), document);
        ConfigService::load(options).expect("load")
    }

    fn load_value(&self, document: &Value) -> ConfigService {
        self.load_inline(document.to_string())
    }
}

/// C1: an explicit override pair reaches the merged document verbatim.
#[test]
fn overrides_reach_the_merged_document_used_by_the_runtime() {
    let service = Fixture::new().load_value(&json!({
        "step_budget": {"max_extensions": 5, "extension_steps": 7}
    }));

    let section = service
        .merged()
        .get("step_budget")
        .expect("step_budget survives normalization; without it the runtime only sees defaults");
    assert_eq!(
        section,
        &json!({"max_extensions": 5, "extension_steps": 7}),
        "the override pair is copied unchanged into the value `BudgetPolicy::from_merged` reads"
    );
}

/// C2: an absent section stays absent, so `BudgetPolicy::default()` keeps
/// owning the documented defaults (15/2/10/3/5/2).
#[test]
fn absent_section_does_not_materialize_defaults() {
    let service = Fixture::new().load_value(&json!({"shell": "/bin/sh"}));

    assert!(
        service.merged().get("step_budget").is_none(),
        "no synthetic section: defaults come from BudgetPolicy::default(), got {:?}",
        service.merged().get("step_budget")
    );
    assert!(
        service.diagnostics().is_empty(),
        "a document without step_budget is not a diagnostic: {:?}",
        service.diagnostics()
    );
}

/// C3: a partial override does not invent the other five keys.
#[test]
fn partial_override_keeps_the_other_keys_absent() {
    let service = Fixture::new().load_value(&json!({"step_budget": {"min_cap": 8}}));

    let section = service
        .merged()
        .get("step_budget")
        .expect("the overridden section survives")
        .as_object()
        .expect("section is an object");
    assert_eq!(
        section.keys().collect::<Vec<_>>(),
        ["min_cap"],
        "only the user's key is present; the rest must fall back to their defaults: {section:?}"
    );
    assert_eq!(section["min_cap"], 8);
}

/// C4a: malformed values are dropped with a diagnostic instead of widening or
/// poisoning the policy. `budget.rs` re-applies the per-key minimums, so the
/// normalizer only enforces "is an unsigned integer".
#[test]
fn malformed_values_are_dropped_with_diagnostics() {
    let service = Fixture::new().load_value(&json!({
        "step_budget": {
            "extension_steps": "many",
            "max_extensions": 2.5,
            "window": -1,
            "thrash_limit": 4
        }
    }));

    let section = service
        .merged()
        .get("step_budget")
        .expect("the well-formed key still survives")
        .as_object()
        .expect("section is an object");
    assert_eq!(
        section,
        &json!({"thrash_limit": 4})
            .as_object()
            .expect("object")
            .clone(),
        "only the unsigned integer is kept: {section:?}"
    );

    let mut paths: Vec<&str> = service
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.kind == DiagnosticKind::Invalid)
        .map(|diagnostic| diagnostic.path.as_str())
        .collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "$.step_budget.extension_steps",
            "$.step_budget.max_extensions",
            "$.step_budget.window"
        ],
        "each rejected value names its own path"
    );
}

/// C4b: a section that is not an object is a diagnostic, not a silently
/// dropped key.
#[test]
fn non_object_section_is_a_diagnostic() {
    let service = Fixture::new().load_value(&json!({"step_budget": 7}));

    assert!(
        service.merged().get("step_budget").is_none(),
        "a scalar section is never copied: {:?}",
        service.merged().get("step_budget")
    );
    let diagnostics = service.diagnostics();
    assert_eq!(diagnostics.len(), 1, "one diagnostic: {diagnostics:?}");
    assert_eq!(diagnostics[0].path, "$.step_budget");
    assert_eq!(diagnostics[0].kind, DiagnosticKind::Invalid);
}

/// The section keeps unknown sub-keys, matching "unknown keys are preserved and
/// ignored, never rejected" (docs/14). `BudgetPolicy::from_merged` skips them.
#[test]
fn unknown_sub_keys_are_preserved_not_rejected() {
    let service = Fixture::new().load_value(&json!({
        "step_budget": {"extension_steps": 4, "not_a_key": true}
    }));

    let section = service
        .merged()
        .get("step_budget")
        .expect("section survives")
        .as_object()
        .expect("section is an object");
    assert_eq!(section["extension_steps"], 4);
    assert_eq!(
        section["not_a_key"], true,
        "an unknown key is preserved and ignored rather than dropped: {section:?}"
    );
}
