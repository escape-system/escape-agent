//! Fail-closed behaviour for markdown agent frontmatter.
//!
//! A markdown agent whose `---` block exists but cannot be parsed must not
//! inherit the permissive defaults: the committed fix replaces its resolved
//! permissions with a single deny-all rule. A file without frontmatter and a
//! file with valid frontmatter keep their resolved permissions.
//!
//! The shipped `steward` agent uses the opposite split: `agents.steward` in
//! `opencode.json` carries the mode, step budget and rules, while the markdown
//! file only carries the prompt. These tests execute that shape, because a
//! loader that drops either half would silently turn a no-edit agent into an
//! editing one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_config::{ConfigService, Effect, LoadOptions};
use serde_json::Value;

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

fn permission_rules(agent: &Value) -> &[Value] {
    agent
        .get("permissions")
        .and_then(Value::as_array)
        .expect("permissions array")
}

/// Position of the first rule matching `action`/`resource`/`effect`.
fn rule_index(rules: &[Value], action: &str, resource: &str, effect: &str) -> Option<usize> {
    rules.iter().position(|rule| {
        rule.get("action").and_then(Value::as_str) == Some(action)
            && rule.get("resource").and_then(Value::as_str) == Some(resource)
            && rule.get("effect").and_then(Value::as_str) == Some(effect)
    })
}

/// The `steward` document entry shipped in `opencode.json`: mode, step budget
/// and all rules live here, not in the markdown file.
const STEWARD_JSON: &str = r#"{
  "agents": {
    "steward": {
      "mode": "subagent",
      "description": "steward",
      "steps": 120,
      "permissions": [
        {"action": "edit", "resource": "*", "effect": "deny"},
        {"action": "subagent", "resource": "*", "effect": "deny"},
        {"action": "shell", "resource": "*", "effect": "allow"}
      ]
    }
  }
}"#;

const STEWARD_MARKDOWN: &str = "---\ndescription: steward\nmode: subagent\n---\nYou are the build steward. You never edit files.\n";

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

#[test]
fn markdown_body_keeps_json_permissions_and_mode() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");
    fixture.write("project/opencode.json", STEWARD_JSON);
    fixture.write("xdg/opencode/agent/steward.md", STEWARD_MARKDOWN);

    let service = fixture.load(&project);

    let agents = service.agents();
    let stewards: Vec<&Value> = agents
        .iter()
        .filter(|agent| agent.get("id").and_then(Value::as_str) == Some("steward"))
        .collect();
    assert_eq!(stewards.len(), 1, "markdown and JSON resolve to one agent");

    let steward = service.agent("steward").expect("steward loads");
    assert_eq!(steward["mode"], "subagent");
    // `to_value` emits `steps` (agents.rs:178), the wire name shared with the
    // config document; `maxSteps` is only an accepted V1 input alias.
    assert_eq!(steward["steps"], 120, "the step budget survives the merge");

    let rules = permission_rules(&steward);
    let edit_deny = rule_index(rules, "edit", "*", "deny")
        .unwrap_or_else(|| panic!("the JSON edit deny is kept: {rules:?}"));
    let subagent_deny = rule_index(rules, "subagent", "*", "deny")
        .unwrap_or_else(|| panic!("the JSON subagent deny is kept: {rules:?}"));
    let shell_allow = rule_index(rules, "shell", "*", "allow")
        .unwrap_or_else(|| panic!("the JSON shell allow is kept: {rules:?}"));
    assert!(
        edit_deny < subagent_deny && subagent_deny < shell_allow,
        "the document's rules keep their declared order, which last-match-wins \
         depends on: edit@{edit_deny} subagent@{subagent_deny} shell@{shell_allow} in {rules:?}"
    );
    // Last-match-wins: the effects the shipped rules intend.
    assert_eq!(
        service.evaluate(Some("steward"), "edit", "crates/oc-config/src/lib.rs"),
        Effect::Deny
    );
    assert_eq!(
        service.evaluate(Some("steward"), "subagent", "worker"),
        Effect::Deny
    );
    assert_eq!(
        service.evaluate(Some("steward"), "shell", "cargo test -p oc-config"),
        Effect::Allow
    );

    let system = steward["system"].as_str().expect("markdown body is system");
    assert!(
        system.contains("You are the build steward. You never edit files."),
        "the markdown body is the agent prompt: {system:?}"
    );
}

#[test]
fn markdown_cannot_set_permissions_and_does_not_erase_them() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");
    fixture.write("project/opencode.json", STEWARD_JSON);
    // `permissions` is not a supported frontmatter key. A one-level mapping is
    // inside the accepted grammar, so this file parses cleanly and takes the
    // "unsupported key is ignored" path, not the fail-closed deny-all path.
    fixture.write(
        "xdg/opencode/agent/steward.md",
        "---\ndescription: steward\nmode: subagent\npermissions:\n  edit: allow\n  subagent: allow\n---\nSteward body.\n",
    );

    let service = fixture.load(&project);
    assert!(
        !service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("unparsable frontmatter")),
        "a mapping-valued permissions key parses; this is the ignored-key path"
    );
    let steward = service.agent("steward").expect("steward loads");
    let rules = permission_rules(&steward);

    assert!(
        rule_index(rules, "edit", "*", "deny").is_some(),
        "the JSON edit deny survives a markdown file: {rules:?}"
    );
    assert!(
        rule_index(rules, "subagent", "*", "deny").is_some(),
        "the JSON subagent deny survives a markdown file: {rules:?}"
    );
    assert_eq!(
        rules
            .iter()
            .filter(|rule| rule["action"] == "edit" && rule["effect"] == "allow")
            .count(),
        0,
        "no frontmatter rule is applied: {rules:?}"
    );
    assert_eq!(
        service.evaluate(Some("steward"), "edit", "src/main.rs"),
        Effect::Deny,
        "a frontmatter permission cannot grant edits"
    );

    // Control: the same document with a markdown file that declares no
    // permissions. The resolved rules must be identical, i.e. the unsupported
    // key neither added nor erased a rule.
    let control = Fixture::new();
    let control_project = control.make_dir("project");
    control.write("project/opencode.json", STEWARD_JSON);
    control.write(
        "xdg/opencode/agent/steward.md",
        "---\ndescription: steward\nmode: subagent\n---\nSteward body.\n",
    );
    let control_service = control.load(&control_project);
    let control_agent = control_service.agent("steward").expect("control");
    let control_rules = permission_rules(&control_agent);
    assert_eq!(
        rules, control_rules,
        "a markdown `permissions:` key is ignored, not merged or reset"
    );
}

#[test]
fn steward_is_not_a_builtin() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");

    let service = fixture.load(&project);
    let agents = service.agents();

    assert_eq!(agents.len(), 7, "the builtin set is unchanged");
    for id in ["steward", "build-master"] {
        assert!(
            !agents
                .iter()
                .any(|agent| agent.get("id").and_then(Value::as_str) == Some(id)),
            "`{id}` must be a project agent, never a builtin"
        );
    }
}

#[test]
fn subagent_mode_makes_the_agent_spawnable() {
    let fixture = Fixture::new();
    let project = fixture.make_dir("project");
    fixture.write("project/opencode.json", STEWARD_JSON);
    fixture.write(
        "xdg/opencode/agent/steward.md",
        "---\ndescription: steward\nmode: subagent\n---\nSteward body.\n",
    );

    let service = fixture.load(&project);
    let steward = service.agent("steward").expect("steward loads");

    // Spawn eligibility as `available_agents` computes it in
    // crates/oc-runtime/src/subagent/settings.rs:201-218:
    // `!hidden && (mode == "subagent" || mode == "all")`. Asserted here on the
    // config JSON so oc-config needs no oc-runtime dependency.
    let mode = steward
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("primary");
    let hidden = steward
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        !hidden && (mode == "subagent" || mode == "all"),
        "`subagent {{agent: \"steward\"}}` must resolve: mode={mode} hidden={hidden}"
    );
}
