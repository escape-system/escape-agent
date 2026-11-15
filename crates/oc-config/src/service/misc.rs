//! Scalar accessors: MCP servers, shell, model, default agent, diagnostics and
//! the normalized documents.

use serde_json::Value;

use crate::ConfigService;
use crate::diagnostic::Diagnostic;

impl ConfigService {
    /// Merged `mcp.servers` object (empty when unconfigured).
    #[must_use]
    pub fn mcp_servers(&self) -> Value {
        self.mcp_servers.clone()
    }

    /// Configured shell, or the platform default.
    #[must_use]
    pub fn shell(&self) -> String {
        self.shell.clone()
    }

    /// Default model reference as `provider/model[#variant]`.
    #[must_use]
    pub fn model(&self) -> Option<String> {
        self.model.clone()
    }

    /// Merged `models` control-plane section (fallback chain, notes, pricing,
    /// quota). Always a JSON object; `{}` when the key is unconfigured.
    ///
    /// Returns an owned clone of the live cell, so a `PUT /api/config/models`
    /// (or [`Self::refresh_models_from_disk`]) is visible to the next caller
    /// without a restart. The lock is held only for the clone.
    #[must_use]
    pub fn models_section(&self) -> Value {
        self.models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .value
            .clone()
    }

    /// Replace the `models` section and bump its revision.
    ///
    /// `models_section` keeps its historical contract of always returning a
    /// JSON object, so a non-object value is normalized to `{}` here rather
    /// than surfacing later as a shape change.
    pub fn set_models(&self, models: Value) {
        let value = if models.is_object() {
            models
        } else {
            Value::Object(serde_json::Map::new())
        };
        let mut cell = self
            .models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cell.value = value;
        cell.revision = cell.revision.wrapping_add(1);
    }

    /// Monotonic revision of the `models` cell. Changes exactly when
    /// [`Self::set_models`] stores a new value; readers that cache a derived
    /// registry compare it to know when to rebuild.
    #[must_use]
    pub fn models_revision(&self) -> u64 {
        self.models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision
    }

    /// Re-read the `models` section from the on-disk global config file.
    ///
    /// Directory precedence, highest first:
    /// 1. `OPENCODE_CONFIG_DIR`
    /// 2. the global directory discovered at load time (`XDG_CONFIG_HOME/opencode`
    ///    or `~/.config/opencode` resolved with the load environment)
    /// 3. `XDG_CONFIG_HOME/opencode`
    /// 4. `HOME/.config/opencode`
    /// 5. `.opencode`
    ///
    /// Inside the directory `<dir>/opencode.jsonc` wins when it exists, else
    /// `<dir>/opencode.json`. There is no create-on-write fallback: a missing
    /// file is an error, not an empty document, because silently clearing the
    /// cell would hide a wrong config directory.
    ///
    /// On any error (missing/unreadable file, unparseable JSONC, non-object
    /// `models`) the cell and its revision are left untouched.
    pub fn refresh_models_from_disk(&self) -> Result<(), String> {
        let dir = self.models_dir();
        let jsonc = dir.join("opencode.jsonc");
        let path = if jsonc.is_file() {
            jsonc
        } else {
            let json = dir.join("opencode.json");
            if !json.is_file() {
                return Err(format!(
                    "no config file found in {} (looked for opencode.jsonc and opencode.json)",
                    dir.display()
                ));
            }
            json
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let document = crate::jsonc::parse_jsonc(&text)
            .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
        let models = match document.get("models") {
            None => Value::Object(serde_json::Map::new()),
            Some(value) if value.is_object() => value.clone(),
            Some(_) => {
                return Err(format!("{}: `models` must be an object", path.display()));
            }
        };
        self.set_models(models);
        Ok(())
    }

    /// Directory [`Self::refresh_models_from_disk`] reads from. See that method
    /// for the precedence.
    fn models_dir(&self) -> std::path::PathBuf {
        if let Some(dir) = std::env::var_os("OPENCODE_CONFIG_DIR") {
            return std::path::PathBuf::from(dir);
        }
        if let Some(dir) = self.global_config_dir() {
            return dir;
        }
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            return std::path::PathBuf::from(dir).join("opencode");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(".config/opencode");
        }
        std::path::PathBuf::from(".opencode")
    }

    /// Selected default primary agent.
    #[must_use]
    pub fn default_agent(&self) -> &str {
        &self.default_agent
    }

    /// Warnings collected while loading: dropped legacy keys, malformed
    /// documents, missing `{file:...}` references.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Normalized documents in load order.
    #[must_use]
    pub fn documents(&self) -> &[Value] {
        &self.documents
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use crate::{ConfigService, LoadOptions};

    /// Load one global document from a throwaway config root.
    ///
    /// The temporary root is dropped before returning; use [`load_in`] when the
    /// test needs to rewrite files and call `refresh_models_from_disk`.
    fn load(document: &str) -> ConfigService {
        let root = tempfile::tempdir().expect("temp dir");
        let service = load_in(root.path(), document);
        drop(root);
        service
    }

    /// Same as [`load`], but the caller keeps the root alive.
    fn load_in(root: &std::path::Path, document: &str) -> ConfigService {
        let home = root.join("home");
        let xdg = root.join("xdg");
        let project = root.join("project");
        for directory in [&home, &xdg, &project] {
            std::fs::create_dir_all(directory).expect("mkdir");
        }
        let global = xdg.join("opencode");
        std::fs::create_dir_all(&global).expect("mkdir");
        if !document.is_empty() {
            std::fs::write(global.join("opencode.json"), document).expect("write config");
        }
        let mut env = HashMap::new();
        for (key, value) in [
            ("HOME", &home),
            ("XDG_CONFIG_HOME", &xdg),
            ("ProgramData", &root.join("programdata")),
        ] {
            env.insert(key.to_owned(), value.to_string_lossy().into_owned());
        }
        env.insert(
            "OPENCODE_CONFIG".to_owned(),
            root.join("missing.json").to_string_lossy().into_owned(),
        );
        // Non-empty but blank, so the process environment cannot leak a document in.
        env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
        ConfigService::load(LoadOptions {
            directory: project,
            env,
        })
        .expect("load")
    }

    #[test]
    fn models_key_survives_load_and_stays_out_of_agents() {
        let service = load(
            r#"{
                "model": "google/gemini-3-pro",
                "agents": {"build": {"model": "opencode-go/glm-5.3-flash"}},
                "models": {
                    "chain": ["google/gemini-3-pro", "opencode-go/glm-5.3-flash"],
                    "notes": {"google/gemini-3-pro": "flat-rate tier"},
                    "pricing": {"google/gemini-3-pro": {"input": 1.25, "cacheRead": 0.3}},
                    "quota": {"reserve_percent": 10}
                }
            }"#,
        );
        let models = service.models_section();
        assert_eq!(
            models["chain"],
            json!(["google/gemini-3-pro", "opencode-go/glm-5.3-flash"]),
            "the new top-level key is preserved instead of being dropped"
        );
        assert_eq!(models["notes"]["google/gemini-3-pro"], "flat-rate tier");
        assert_eq!(models["pricing"]["google/gemini-3-pro"]["cacheRead"], 0.3);
        assert_eq!(models["quota"]["reserve_percent"], 10);
        assert!(
            models.get("agents").is_none(),
            "models must not absorb the agents section"
        );
        assert_eq!(service.model(), Some("google/gemini-3-pro".to_owned()));
        // `agents.build` is a legacy alias: normalization remaps it onto the
        // canonical `engineer` id instead of creating a ghost custom agent.
        assert_eq!(
            service.merged()["agents"]["engineer"]["model"],
            json!({"providerID": "opencode-go", "model": "glm-5.3-flash"}),
            "agent normalization remaps the build alias"
        );
        assert!(
            service.merged()["agents"].get("build").is_none(),
            "the legacy build key must not survive normalization"
        );
    }

    #[test]
    fn models_section_defaults_to_an_empty_object() {
        let service = load("");
        assert_eq!(service.models_section(), json!({}));
        assert_eq!(service.models_revision(), 0, "loading is revision zero");
    }

    #[test]
    fn set_models_replaces_the_cell_and_bumps_the_revision() {
        let service = load(r#"{"models": {"chain": ["openai/gpt-5"]}}"#);
        assert_eq!(service.models_section()["chain"], json!(["openai/gpt-5"]));
        let before = service.models_revision();

        service.set_models(json!({"chain": ["anthropic/claude-opus-4-6"]}));
        assert_eq!(
            service.models_section()["chain"],
            json!(["anthropic/claude-opus-4-6"]),
            "readers see the new cell without a reload"
        );
        assert!(
            service.models_revision() > before,
            "the revision advances so a cached registry rebuilds"
        );

        let bumped = service.models_revision();
        service.set_models(json!("not an object"));
        assert_eq!(
            service.models_section(),
            json!({}),
            "a non-object patch keeps the always-an-object contract"
        );
        assert!(
            service.models_revision() > bumped,
            "the revision tracks the write even when it is normalized"
        );
    }

    #[test]
    fn refresh_models_from_disk_updates_the_cell_and_revision() {
        let root = tempfile::tempdir().expect("temp dir");
        let service = load_in(root.path(), r#"{"models": {"chain": ["openai/gpt-5"]}}"#);
        let before = service.models_revision();

        std::fs::write(
            root.path().join("xdg/opencode/opencode.json"),
            r#"{"models": {"chain": ["google/gemini-3-pro"], "notes": {"a": "b"}}}"#,
        )
        .expect("rewrite config");
        service.refresh_models_from_disk().expect("refresh");

        assert_eq!(
            service.models_section()["chain"],
            json!(["google/gemini-3-pro"])
        );
        assert_eq!(service.models_section()["notes"]["a"], "b");
        assert!(service.models_revision() > before);
    }

    #[test]
    fn refresh_reports_a_bad_file_and_leaves_the_cell_alone() {
        let root = tempfile::tempdir().expect("temp dir");
        let service = load_in(root.path(), r#"{"models": {"chain": ["openai/gpt-5"]}}"#);
        let before = (service.models_section(), service.models_revision());

        std::fs::remove_file(root.path().join("xdg/opencode/opencode.json")).expect("remove");
        let error = service
            .refresh_models_from_disk()
            .expect_err("missing file");
        assert!(
            error.contains("no config file found"),
            "the error names the problem: {error}"
        );
        assert_eq!(
            (service.models_section(), service.models_revision()),
            before,
            "a failed refresh must not clear the cell or bump the revision"
        );

        // A malformed document and a non-object `models` value fail the same way.
        for document in [r#"{"models": "#, r#"{"models": ["not", "an", "object"]}"#] {
            std::fs::write(root.path().join("xdg/opencode/opencode.json"), document)
                .expect("write");
            assert!(
                service.refresh_models_from_disk().is_err(),
                "expected an error for {document}"
            );
            assert_eq!(
                (service.models_section(), service.models_revision()),
                before,
                "an invalid document leaves the cell untouched: {document}"
            );
        }
    }

    #[test]
    fn refresh_prefers_jsonc_when_both_files_exist() {
        let root = tempfile::tempdir().expect("temp dir");
        let service = load_in(root.path(), r#"{"models": {"chain": ["openai/gpt-5"]}}"#);
        let global = root.path().join("xdg/opencode");

        std::fs::write(
            global.join("opencode.json"),
            r#"{"models": {"chain": ["from-json/model"]}}"#,
        )
        .expect("write json");
        std::fs::write(
            global.join("opencode.jsonc"),
            "{\n  // the write target wins\n  \"models\": {\"chain\": [\"from-jsonc/model\"]},\n}\n",
        )
        .expect("write jsonc");

        service.refresh_models_from_disk().expect("refresh");
        assert_eq!(
            service.models_section()["chain"],
            json!(["from-jsonc/model"]),
            "opencode.jsonc is the write target when it exists"
        );
    }
}
