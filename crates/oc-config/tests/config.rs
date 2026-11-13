//! End-to-end tests for discovery precedence, JSONC, substitution, V1 aliases,
//! agents and permission evaluation. Every test uses its own temporary
//! directory and env map; nothing touches real user configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_config::{ConfigService, Effect, EntryKind, LoadOptions};
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
        // `LoadOptions` layers over the process environment, so a runner that
        // exports these (an agent session does) would otherwise inject a real
        // document into every fixture. Whitespace is non-empty here (empty
        // values fall through to the process env) and is skipped by loading.
        env.insert("OPENCODE_CONFIG_CONTENT".to_owned(), " ".to_owned());
        LoadOptions {
            directory: directory.to_path_buf(),
            env,
        }
    }

    fn load(&self, directory: &Path) -> ConfigService {
        ConfigService::load(self.options(directory)).expect("load")
    }
}

fn get<'a>(value: &'a Value, path: &[&str]) -> &'a Value {
    let mut current = value;
    for key in path {
        current = current.get(*key).unwrap_or_else(|| panic!("missing {key}"));
    }
    current
}

#[test]
fn precedence_low_to_high_matches_discovery_order() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let global = fixture.write(
        "xdg/opencode/opencode.json",
        r#"{"shell": "/bin/global", "model": "openai/gpt-5", "username": "global"}"#,
    );
    let direct = fixture.write(
        "project/opencode.json",
        r#"{"shell": "/bin/project-direct"}"#,
    );
    let nested = fixture.write(
        "project/.opencode/opencode.json",
        r#"{"shell": "/bin/project-opencode"}"#,
    );

    let service = fixture.load(&project);

    assert_eq!(service.merged()["shell"], "/bin/project-opencode");
    assert_eq!(service.model().as_deref(), Some("openai/gpt-5"));
    assert_eq!(service.merged()["username"], "global");
    assert_eq!(service.shell(), "/bin/project-opencode");

    let position = |target: &Path| {
        service
            .entries()
            .iter()
            .position(|entry| entry.path.as_deref().map(Path::new) == Some(target))
            .unwrap_or_else(|| panic!("missing entry {}", target.display()))
    };
    assert!(position(&global) < position(&direct));
    assert!(position(&direct) < position(&nested));
    let global_directory = service
        .entries()
        .iter()
        .position(|entry| {
            entry.kind == EntryKind::Directory
                && entry.path.as_deref().map(Path::new)
                    == Some(fixture.path("xdg/opencode").as_path())
        })
        .expect("global directory entry");
    assert!(position(&global) < global_directory);
    assert!(global_directory < position(&direct));

    // Wire shape has no nulls.
    let value = serde_json::to_value(&service.entries()[global_directory]).expect("serialize");
    assert_eq!(value["type"], "directory");
    assert!(value.get("info").is_none());
}

#[test]
fn explicit_file_and_inline_content_precedence() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write("project/opencode.json", r#"{"shell": "/bin/project"}"#);
    let explicit = fixture.write(
        "explicit.json",
        r#"{"shell": "/bin/explicit", "username": "explicit-user"}"#,
    );

    let mut options = fixture.options(&project);
    options.env.insert(
        "OPENCODE_CONFIG".to_owned(),
        explicit.to_string_lossy().into_owned(),
    );
    let service = ConfigService::load(options).expect("load");
    assert_eq!(service.merged()["shell"], "/bin/project");
    assert_eq!(service.merged()["username"], "explicit-user");

    let mut options = fixture.options(&project);
    options.env.insert(
        "OPENCODE_CONFIG_CONTENT".to_owned(),
        r#"{"shell": "/bin/content"}"#.to_owned(),
    );
    let service = ConfigService::load(options).expect("load");
    assert_eq!(service.merged()["shell"], "/bin/content");
}

#[test]
fn jsonc_comments_trailing_commas_and_broken_siblings() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write("project/opencode.json", r#"{"shell": "/bin/broken"#);
    fixture.write(
        "project/opencode.jsonc",
        r#"{
  // comments are allowed
  "shell": "/bin/jsonc",
  "instructions": ["a.md", "b.md",],
}
"#,
    );

    let service = fixture.load(&project);
    assert_eq!(service.merged()["shell"], "/bin/jsonc");
    assert_eq!(service.merged()["instructions"][1], "b.md");
    // The malformed document is skipped with a diagnostic and does not kill the load.
    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.path == "$")
    );
    let documents = service
        .entries()
        .iter()
        .filter(|entry| entry.kind == EntryKind::Document)
        .count();
    assert_eq!(documents, 1);
}

#[test]
fn substitution_escapes_env_only_when_needed_and_skips_comments() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write("project/name.txt", "  agent name  \n");
    fixture.write(
        "project/opencode.jsonc",
        r#"{
  // {file:commented-out.txt} must stay untouched
  "shell": "{env:ODD_SHELL}",
  "username": "{file:name.txt}",
  "timezone": "{file:missing.txt}"
}
"#,
    );

    let mut options = fixture.options(&project);
    options.env.insert(
        "ODD_SHELL".to_owned(),
        "C:\\Program Files\\pwsh.exe".to_owned(),
    );
    let service = ConfigService::load(options).expect("load");

    assert_eq!(service.merged()["shell"], "C:\\Program Files\\pwsh.exe");
    assert_eq!(service.merged()["username"], "agent name");
    assert_eq!(service.merged()["timezone"], "");
    let missing: Vec<_> = service
        .diagnostics()
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("{file:") || diagnostic.message.contains("file reference")
        })
        .collect();
    assert_eq!(missing.len(), 1);
    assert!(missing[0].message.contains("missing.txt"));
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture exercises every V1 alias; splitting would fragment the assertions.
fn v1_aliases_are_normalized_and_dropped_keys_warn() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write(
        "project/opencode.json",
        r#"{
  "provider": {"openai": {"npm": "@ai-sdk/openai", "name": "OpenAI"}},
  "agent": {
    "custom": {
      "prompt": "be nice",
      "model": "openai/gpt-5",
      "tools": {"write": true, "bash": false},
      "permission": {"read": "allow"},
      "mode": "subagent"
    }
  },
  "mode": {"planner": {"prompt": "plan"}},
  "plugin": [["some-plugin", {"a": 1}], "other"],
  "permission": {"bash": {"git status": "allow"}, "task": "deny"},
  "tools": {"patch": false},
  "command": {"hello": {"template": "hi", "model": "openai/gpt-5", "subtask": true}},
  "reference": {"docs": "/tmp/docs"},
  "attachment": {"image": {"max_width": 100}},
  "snapshot": false,
  "autoupdate": true,
  "autoshare": true,
  "small_model": "openai/gpt-4o-mini",
  "enabled_providers": ["azure-cognitive-services"],
  "disabled_providers": ["anthropic"],
  "experimental": {"mcp_timeout": 5000, "disable_paste_summary": true, "todo": true},
  "logLevel": "debug",
  "layout": "tabs"
}
"#,
    );

    let service = fixture.load(&project);
    let merged = service.merged();

    assert_eq!(
        get(merged, &["providers", "openai", "package"]),
        "@ai-sdk/openai"
    );
    assert_eq!(get(merged, &["agents", "custom", "system"]), "be nice");
    assert_eq!(get(merged, &["agents", "custom", "mode"]), "subagent");
    assert_eq!(
        get(merged, &["agents", "custom", "model", "providerID"]),
        "openai"
    );
    let custom = get(merged, &["agents", "custom", "permissions"])
        .as_array()
        .expect("permissions");
    assert!(custom.iter().any(|rule| {
        rule["action"] == "edit" && rule["effect"] == "allow" && rule["resource"] == "*"
    }));
    assert!(custom.iter().any(|rule| {
        rule["action"] == "shell" && rule["effect"] == "deny" && rule["resource"] == "*"
    }));

    assert_eq!(get(merged, &["agents", "planner", "mode"]), "primary");
    assert_eq!(get(merged, &["agents", "planner", "system"]), "plan");
    assert_eq!(
        get(merged, &["agents", "title", "model", "model"]),
        "gpt-4o-mini"
    );

    let plugins = merged["plugins"].as_array().expect("plugins");
    assert_eq!(plugins.len(), 2);
    assert_eq!(plugins[0]["package"], "some-plugin");
    assert_eq!(plugins[0]["options"]["a"], 1);
    assert_eq!(plugins[1], "other");

    let rules = service.permissions();
    assert_eq!(rules[0].action, "edit");
    assert_eq!(rules[0].effect, Effect::Deny);
    assert!(rules.iter().any(|rule| {
        rule.action == "shell" && rule.resource == "git status" && rule.effect == Effect::Allow
    }));
    assert!(rules.iter().any(|rule| {
        rule.action == "subagent" && rule.resource == "*" && rule.effect == Effect::Deny
    }));

    assert_eq!(get(merged, &["commands", "hello", "subagent"]), true);
    assert_eq!(
        get(merged, &["commands", "hello", "model", "model"]),
        "gpt-5"
    );
    assert_eq!(get(merged, &["references", "docs"]), "/tmp/docs");
    assert_eq!(get(merged, &["media", "image", "max_width"]), 100);
    assert_eq!(merged["snapshots"], false);
    assert_eq!(merged["update"], "auto");
    assert_eq!(merged["share"], "auto");

    let policies = merged["policies"].as_array().expect("policies");
    assert_eq!(policies[0]["action"], "provider.use");
    assert_eq!(policies[0]["resource"], "*");
    assert_eq!(policies[0]["effect"], "deny");
    assert!(
        policies
            .iter()
            .any(|policy| { policy["resource"] == "azure" && policy["effect"] == "allow" })
    );
    assert!(
        policies
            .iter()
            .any(|policy| { policy["resource"] == "anthropic" && policy["effect"] == "deny" })
    );

    assert_eq!(get(merged, &["mcp", "timeout", "catalog"]), 5000);
    assert!(get(merged, &["mcp", "timeout"]).get("startup").is_none());
    assert_eq!(get(merged, &["experimental", "todo"]), true);
    assert!(
        get(merged, &["experimental"])
            .get("disable_paste_summary")
            .is_none()
    );

    for dropped in ["$.logLevel", "$.layout"] {
        assert!(
            service
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.path == dropped),
            "expected diagnostic for {dropped}"
        );
    }
    assert!(
        service
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.path == "$.experimental.disable_paste_summary" })
    );
    // Translated keys must not leak through as unknown keys.
    assert!(merged.get("provider").is_none());
    assert!(merged.get("tools").is_none());
    assert!(merged.get("permission").is_none());
}

#[test]
fn permissions_compose_and_last_match_wins() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write(
        "project/opencode.json",
        r#"{
  "permissions": [
    {"action": "read", "resource": "*", "effect": "deny"},
    {"action": "read", "resource": "src/*", "effect": "allow"},
    {"action": "read", "resource": "*.env", "effect": "deny"},
    {"action": "read", "resource": "*.env.example", "effect": "allow"}
  ],
  "agents": {
    "build": {
      "permissions": [
        {"action": "read", "resource": "src/secret/*", "effect": "deny"}
      ]
    }
  }
}
"#,
    );

    let service = fixture.load(&project);

    // Document rules apply, then the agent's own rules.
    assert_eq!(
        service.evaluate(Some("build"), "read", "src/main.rs"),
        Effect::Allow
    );
    assert_eq!(
        service.evaluate(Some("build"), "read", "src/secret/key"),
        Effect::Deny
    );
    assert_eq!(
        service.evaluate(Some("build"), "read", "config.env.example"),
        Effect::Allow
    );
    assert_eq!(
        service.evaluate(Some("build"), "read", "config.env"),
        Effect::Deny
    );
    // Without an agent, only document rules apply.
    assert_eq!(service.evaluate(None, "read", "src/lib.rs"), Effect::Allow);
    assert_eq!(service.evaluate(None, "write", "src/lib.rs"), Effect::Ask);
    // Unknown agents fall back to document rules.
    assert_eq!(
        service.evaluate(Some("nope"), "write", "src/lib.rs"),
        Effect::Ask
    );
}

#[test]
fn wildcard_matching_uses_star_and_question() {
    assert!(oc_config::wildcard_match("src/main.rs", "src/?ain.rs"));
    assert!(oc_config::wildcard_match("any/path/at/all", "*"));
    assert!(oc_config::wildcard_match("a\\b", "a/b"));
    assert!(!oc_config::wildcard_match("src/mine.rs", "src/?ain.rs"));
    assert_eq!(
        oc_config::evaluate_rules(
            &[oc_config::PermissionRule::new(
                "read",
                "*.env*",
                Effect::Deny
            )],
            "read",
            "prod.env"
        ),
        Effect::Deny
    );
}

#[test]
fn builtin_agents_have_upstream_modes_permissions_and_prompts() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write("project/opencode.json", r#"{"default_agent": "plan"}"#);
    let service = fixture.load(&project);

    let agents = service.agents();
    assert_eq!(agents.len(), 7);
    assert_eq!(agents[0]["id"], "plan");

    let build = service.agent("build").expect("build");
    assert_eq!(build["mode"], "primary");
    assert_eq!(build["hidden"], false);
    assert!(build.get("model").is_none());
    assert!(build.get("system").is_none());
    assert_eq!(build["request"]["settings"], serde_json::json!({}));

    let general = service.agent("general").expect("general");
    assert_eq!(general["mode"], "subagent");
    let rules = general["permissions"].as_array().expect("rules");
    assert!(
        rules
            .iter()
            .any(|rule| rule["action"] == "question" && rule["effect"] == "deny")
    );
    assert!(
        !rules
            .iter()
            .any(|rule| rule["action"] == "subagent" && rule["effect"] == "deny"),
        "general has no subagent deny; depth and allow policy decide at spawn time: {rules:?}"
    );

    let explore = service.agent("explore").expect("explore");
    assert_eq!(explore["mode"], "subagent");
    assert!(
        explore["system"]
            .as_str()
            .expect("system")
            .starts_with("You are a file search specialist.")
    );
    let rules = explore["permissions"].as_array().expect("rules");
    assert!(
        rules
            .iter()
            .any(|rule| rule["action"] == "*" && rule["effect"] == "deny")
    );
    assert!(
        rules
            .iter()
            .any(|rule| rule["action"] == "subagent" && rule["effect"] == "deny"),
        "explore stays read-only and cannot spawn: {rules:?}"
    );

    let title = service.agent("title").expect("title");
    assert_eq!(title["hidden"], true);
    assert!(
        title["system"]
            .as_str()
            .expect("system")
            .contains("You name conversations")
    );
    let summary = service.agent("summary").expect("summary");
    assert_eq!(summary["hidden"], true);
    let compaction = service.agent("compaction").expect("compaction");
    assert_eq!(compaction["hidden"], true);
    for agent in [title, summary, compaction] {
        let rules = agent["permissions"].as_array().expect("rules");
        assert!(
            rules
                .iter()
                .any(|rule| rule["action"] == "*" && rule["effect"] == "deny")
        );
    }

    let plan = service.agent("plan").expect("plan");
    let rules = plan["permissions"].as_array().expect("rules");
    assert!(
        rules
            .iter()
            .any(|rule| rule["action"] == "edit" && rule["effect"] == "deny")
    );
    assert!(rules.iter().any(|rule| {
        rule["action"] == "edit"
            && rule["effect"] == "allow"
            && rule["resource"]
                .as_str()
                .is_some_and(|resource| resource.contains(".opencode") && resource.ends_with('*'))
    }));
}

/// `code_map` is a read-only search tool; the explore subagent must be able to
/// see it (the runtime drops every tool whose action the agent denies), while
/// the write-capable actions stay denied.
///
/// Regression for the explore toolset omitting `code_map` from its allow list
/// while `deny("*","*")` hides everything not allowed explicitly.
#[test]
fn explore_toolset_allows_read_only_code_map_and_still_denies_edits() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let service = fixture.load(&project);

    assert_eq!(
        service.evaluate(Some("explore"), "code_map", "*"),
        Effect::Allow,
        "explore must see code_map: its action is the tool name"
    );
    // `build` composes the default `* * allow`, so code_map stays visible.
    assert_eq!(
        service.evaluate(Some("build"), "code_map", "*"),
        Effect::Allow
    );
    for action in ["read", "grep", "glob", "webfetch", "websearch", "shell"] {
        assert_eq!(
            service.evaluate(Some("explore"), action, "*"),
            Effect::Allow,
            "explore keeps its read-only search toolset: {action}"
        );
    }
    for action in ["edit", "write", "patch"] {
        assert_eq!(
            service.evaluate(Some("explore"), action, "src/main.rs"),
            Effect::Deny,
            "explore must not gain write-capable tools: {action}"
        );
    }
}

#[test]
fn broken_documents_are_diagnostics_not_failures() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write("project/opencode.json", r#"{"shell": "#);
    fixture.write(
        "project/.opencode/opencode.json",
        r#"["not", "an", "object"]"#,
    );
    fixture.write(
        "project/.opencode/opencode.jsonc",
        r#"{"shell": "/bin/ok"}"#,
    );

    let service = fixture.load(&project);
    assert_eq!(service.merged()["shell"], "/bin/ok");
    let diagnostics = service.diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.kind == oc_config::DiagnosticKind::Invalid)
    );
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("root is not an object"))
    );
}

#[test]
fn configured_agent_overrides_and_custom_agents() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write(
        "project/opencode.json",
        r#"{
  "permissions": [{"action": "shell", "resource": "*", "effect": "allow"}],
  "agents": {
    "build": {"description": "custom build", "steps": 5, "model": "openai/gpt-5#high"},
    "helper": {"description": "custom helper", "mode": "subagent", "hidden": true}
  }
}
"#,
    );
    let service = fixture.load(&project);

    let build = service.agent("build").expect("build");
    assert_eq!(build["description"], "custom build");
    assert_eq!(build["steps"], 5);
    assert_eq!(build["model"]["id"], "gpt-5");
    assert_eq!(build["model"]["providerID"], "openai");
    assert_eq!(build["model"]["variant"], "high");
    // Config documents keep the selection shape.
    assert_eq!(
        service.merged()["agents"]["build"]["model"]["model"],
        "gpt-5"
    );
    assert_eq!(
        service.evaluate(Some("build"), "shell", "cargo test"),
        Effect::Allow
    );

    let helper = service.agent("helper").expect("helper");
    assert_eq!(helper["mode"], "subagent");
    assert_eq!(helper["hidden"], true);
    assert_eq!(helper["name"], "helper");
    // Custom agents are not selectable defaults, and build remains first.
    assert_eq!(service.agents()[0]["id"], "build");
    assert_eq!(service.default_agent(), "build");
}

#[test]
fn symbol_edit_permission_alias_normalizes_to_edit() {
    let fixture = Fixture::new();
    let project = fixture.project();
    fixture.write(
        "project/opencode.json",
        r#"{
  "permission": {
    "symbol_edit": "deny"
  }
}
"#,
    );
    let service = fixture.load(&project);
    assert_eq!(
        service.evaluate(Some("build"), "edit", "src/lib.rs"),
        Effect::Deny
    );
    assert_eq!(
        service.evaluate(Some("explore"), "edit", "src/lib.rs"),
        Effect::Deny
    );
    assert_eq!(
        service.evaluate(Some("explore"), "read", "src/lib.rs"),
        Effect::Allow
    );
}
