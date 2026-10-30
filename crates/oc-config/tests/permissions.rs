//! Behavior of ordered permission rules: bare-action matching, rule ordering,
//! wildcard resource normalization/case folding and the built-in read defaults.
//!
//! Every test is offline and reads no real user configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oc_config::{
    ConfigService, Effect, LoadOptions, PermissionRule, evaluate_rules, wildcard_match,
};

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
        // Whitespace is non-empty (empty values fall through to the process
        // env) and skipped by loading, so an agent session cannot inject a
        // real document into the fixture.
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

/// Upstream turns a trailing ` *` in an action pattern into `( .*)?`, so the
/// bare action `read` matches `read *` but `readfile` does not.
#[test]
fn bare_action_matches_trailing_space_star() {
    assert!(wildcard_match("read", "read *"));
    assert!(wildcard_match("read file.txt", "read *"));
    assert!(!wildcard_match("readfile", "read *"));

    let rules = [
        PermissionRule::new("*", "*", Effect::Allow),
        PermissionRule::new("read *", "*", Effect::Deny),
    ];
    assert_eq!(evaluate_rules(&rules, "read", "x"), Effect::Deny);
    assert_eq!(evaluate_rules(&rules, "readfile", "x"), Effect::Allow);
    // No rule at all is the implicit `ask`.
    assert_eq!(
        evaluate_rules(Vec::<&PermissionRule>::new(), "read", "x"),
        Effect::Ask
    );
}

/// `evaluate_rules` is last-match-wins: there is no separate deny priority, so
/// swapping two rules with identical action/resource swaps the outcome.
#[test]
fn the_last_matching_rule_decides_without_deny_priority() {
    let deny = PermissionRule::new("read", "*", Effect::Deny);
    let allow = PermissionRule::new("read", "*", Effect::Allow);

    assert_eq!(
        evaluate_rules([&deny, &allow], "read", "x"),
        Effect::Allow,
        "a later allow overrides an earlier deny"
    );
    assert_eq!(
        evaluate_rules([&allow, &deny], "read", "x"),
        Effect::Deny,
        "a later deny overrides an earlier allow"
    );
    // A non-matching action leaves the implicit default untouched.
    assert_eq!(evaluate_rules([&allow, &deny], "write", "x"), Effect::Ask);
}

/// Resources compare over `/`-normalized strings; case folds only on Windows,
/// mirroring upstream `util/wildcard.ts`.
#[test]
fn wildcard_resources_normalize_separators_and_follow_platform_case() {
    assert!(wildcard_match("a\\b", "a/b"));
    assert!(wildcard_match("a/b", "a\\b"));
    assert_eq!(
        wildcard_match("SRC\\Main.RS", "src/main.rs"),
        cfg!(windows),
        "case-insensitive only on Windows"
    );
    assert_eq!(wildcard_match("README.md", "readme.md"), cfg!(windows));

    let rules = [PermissionRule::new("read", "src/*", Effect::Allow)];
    assert_eq!(
        evaluate_rules(&rules, "read", "src\\deep\\main.rs"),
        Effect::Allow
    );
    assert_eq!(evaluate_rules(&rules, "read", "docs\\main.rs"), Effect::Ask);
    if cfg!(windows) {
        assert_eq!(
            evaluate_rules(&rules, "READ", "SRC\\deep\\MAIN.RS"),
            Effect::Allow
        );
    }
}

/// With no configuration document, document-level evaluation asks; the default
/// agent's built-in rules allow reads but ask for `.env` files, and the later
/// `*.env.example` allow wins over the earlier `*.env.*` ask.
#[test]
fn empty_config_uses_builtin_read_defaults_for_the_default_agent() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let service = fixture.load(&project);

    assert_eq!(
        service.evaluate(None, "read", "src/main.rs"),
        Effect::Ask,
        "an empty document has no document-level rules"
    );

    let agent = Some(service.default_agent());
    assert_eq!(service.evaluate(agent, "read", "a.env"), Effect::Ask);
    assert_eq!(service.evaluate(agent, "read", "a.env.local"), Effect::Ask);
    assert_eq!(
        service.evaluate(agent, "read", "a.env.example"),
        Effect::Allow
    );
    assert_eq!(
        service.evaluate(agent, "read", "src/main.rs"),
        Effect::Allow
    );
}
