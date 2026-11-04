//! Fail-closed behaviour for markdown agent frontmatter.
//!
//! A markdown agent whose `---` block exists but cannot be parsed must not
//! inherit the permissive defaults: the committed fix replaces its resolved
//! permissions with a single deny-all rule. A file without frontmatter and a
//! file with valid frontmatter keep their resolved permissions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_config::{ConfigService, Effect, LoadOptions};

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

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, content).expect("write");
        path
    }

    fn make_dir(&self, relative: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(&path).expect("mkdir");
        path
    }

    fn load(&self, directory: &Path) -> ConfigService {
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
        // Shadow runner-exported inline config so a real session cannot inject
        // documents into fixtures.
        env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
        ConfigService::load(LoadOptions {
            directory: directory.to_path_buf(),
            env,
        })
        .expect("load")
    }
}

fn permission_count(service: &ConfigService, id: &str) -> usize {
    service
        .agent(id)
        .expect("agent")
        .get("permissions")
        .and_then(|value| value.as_array())
        .map_or(0, Vec::len)
}

#[test]
fn unparsable_frontmatter_denies_everything() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");

    // Invalid YAML: a flow collection which the supported frontmatter subset
    // rejects. The file still loads as an agent, and the declaration it could
    // not parse must not degrade into the permissive defaults.
    fixture.write(
        "xdg/opencode/agent/broken.md",
        "---\ndescription: broken\npermissions: [deny edit *\n---\nbody\n",
    );

    let service = fixture.load(&project);

    assert!(
        service.agent("broken").is_some(),
        "the agent is still discovered despite invalid frontmatter"
    );
    assert_eq!(
        service.evaluate(Some("broken"), "edit", "anything"),
        Effect::Deny,
        "unparsable frontmatter must fail closed"
    );
    assert_eq!(
        service.evaluate(Some("broken"), "read", "anything"),
        Effect::Deny,
        "deny-all covers actions the file never mentioned"
    );
    assert_eq!(
        permission_count(&service, "broken"),
        1,
        "the resolved permissions are replaced by a single deny-all rule"
    );
    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unparsable frontmatter")),
        "the parse failure is reported"
    );
}

#[test]
fn no_frontmatter_still_loads() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");
    fixture.write(
        "xdg/opencode/agent/plain.md",
        "A plain agent body with no frontmatter.\n",
    );

    let service = fixture.load(&project);

    assert!(service.agent("plain").is_some(), "plain agent loads");
    assert_ne!(
        service.evaluate(Some("plain"), "read", "x"),
        Effect::Deny,
        "a file without frontmatter keeps the permissive defaults"
    );
    assert!(
        permission_count(&service, "plain") > 1,
        "a blanket deny would collapse the inherited rules to one entry"
    );
}

#[test]
fn valid_frontmatter_keeps_its_permissions() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");

    // Document permissions are part of the agent's resolved rule set. The
    // markdown file's valid frontmatter must leave them intact.
    fixture.write(
        "xdg/opencode/opencode.json",
        r#"{"permissions": [{"action": "edit", "resource": "*", "effect": "deny"}]}"#,
    );
    fixture.write(
        "xdg/opencode/agent/checked.md",
        "---\ndescription: checked\nmode: subagent\n---\nChecked.\n",
    );

    let service = fixture.load(&project);

    assert_eq!(
        service.evaluate(Some("checked"), "edit", "src/main.rs"),
        Effect::Deny,
        "the document-level edit deny is inherited"
    );
    assert_ne!(
        service.evaluate(Some("checked"), "read", "src/main.rs"),
        Effect::Deny,
        "valid frontmatter must not trigger the deny-all fallback"
    );
    assert!(
        permission_count(&service, "checked") > 1,
        "valid frontmatter keeps the resolved rules"
    );
}
