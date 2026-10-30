//! Ordered permission rules, wildcard matching and evaluation.
//!
//! Evaluation is last-match-wins with `ask` as the implicit default. Matching
//! supports `*` and `?`; the comparison is case-insensitive only on Windows,
//! mirroring upstream `util/wildcard.ts`.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
    #[default]
    Ask,
}

impl Effect {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "ask" => Some(Self::Ask),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

impl PermissionRule {
    #[must_use]
    pub fn new(action: impl Into<String>, resource: impl Into<String>, effect: Effect) -> Self {
        Self {
            action: action.into(),
            resource: resource.into(),
            effect,
        }
    }
}

/// The default rule set of every built-in agent (upstream `Agent.Info.default`).
#[must_use]
pub(crate) fn default_rules() -> Vec<PermissionRule> {
    vec![
        PermissionRule::new("*", "*", Effect::Allow),
        PermissionRule::new("read", "*.env", Effect::Ask),
        PermissionRule::new("read", "*.env.*", Effect::Ask),
        PermissionRule::new("read", "*.env.example", Effect::Allow),
    ]
}

/// Expand `~`/`$HOME` for actions whose resources are filesystem paths.
pub(crate) fn expand_home(rules: &mut [PermissionRule], home: &Path) {
    let home = home.to_string_lossy();
    if home.is_empty() {
        return;
    }
    for rule in rules {
        if !matches!(rule.action.as_str(), "read" | "edit" | "external_directory") {
            continue;
        }
        rule.resource = expand_resource(&rule.resource, &home);
    }
}

fn expand_resource(resource: &str, home: &str) -> String {
    if resource == "~" || resource == "$HOME" {
        return home.to_owned();
    }
    let relative = resource.strip_prefix("~/").or_else(|| {
        resource
            .strip_prefix("$HOME/")
            .or_else(|| resource.strip_prefix("$HOME\\"))
    });
    match relative {
        Some(relative) => {
            let mut joined = std::path::PathBuf::from(home);
            joined.push(relative);
            joined.to_string_lossy().into_owned()
        }
        None => resource.to_owned(),
    }
}

/// Evaluate ordered rules; the last matching rule wins, no match means `ask`.
#[must_use]
pub fn evaluate_rules<'a, I>(rules: I, action: &str, resource: &str) -> Effect
where
    I: IntoIterator<Item = &'a PermissionRule>,
{
    let mut effect = Effect::Ask;
    for rule in rules {
        if wildcard_match(action, &rule.action) && wildcard_match(resource, &rule.resource) {
            effect = rule.effect;
        }
    }
    effect
}

/// Upstream-compatible wildcard match over `/`-normalized strings.
#[must_use]
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let input: Vec<char> = normalize_separators(input).chars().collect();
    let pattern: Vec<char> = normalize_separators(pattern).chars().collect();
    let case_insensitive = cfg!(windows);
    if glob(&pattern, &input, case_insensitive) {
        return true;
    }
    // Upstream turns a trailing ` *` into `( .*)?`, so `read *` also matches `read`.
    if pattern.len() >= 2 && pattern[pattern.len() - 1] == '*' && pattern[pattern.len() - 2] == ' '
    {
        return glob(&pattern[..pattern.len() - 2], &input, case_insensitive);
    }
    false
}

fn normalize_separators(value: &str) -> String {
    value.replace('\\', "/")
}

fn glob(pattern: &[char], text: &[char], case_insensitive: bool) -> bool {
    let mut p = 0;
    let mut t = 0;
    let mut star: Option<usize> = None;
    let mut resume = 0;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = t;
            p += 1;
        } else if p < pattern.len() && char_matches(pattern[p], text[t], case_insensitive) {
            p += 1;
            t += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            resume += 1;
            t = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn char_matches(pattern: char, value: char, case_insensitive: bool) -> bool {
    if pattern == '?' {
        return true;
    }
    if pattern == value {
        return true;
    }
    case_insensitive && pattern.eq_ignore_ascii_case(&value)
}

#[cfg(test)]
mod tests {
    use super::{Effect, PermissionRule, evaluate_rules, wildcard_match};

    #[test]
    fn wildcards_and_last_match_wins() {
        assert!(wildcard_match("read", "read"));
        assert!(wildcard_match("anything/at/all", "*"));
        assert!(wildcard_match("src/main.rs", "src/?ain.rs"));
        assert!(!wildcard_match("src/mine.rs", "src/?ain.rs"));
        assert!(wildcard_match("a\\b", "a/b"));

        let rules = vec![
            PermissionRule::new("read", "*", Effect::Allow),
            PermissionRule::new("read", "*.env", Effect::Deny),
            PermissionRule::new("shell", "*", Effect::Allow),
        ];
        assert_eq!(evaluate_rules(&rules, "read", "config.env"), Effect::Deny);
        assert_eq!(evaluate_rules(&rules, "read", "config.txt"), Effect::Allow);
        assert_eq!(evaluate_rules(&rules, "write", "config.txt"), Effect::Ask);

        #[cfg(windows)]
        assert!(wildcard_match("README.MD", "readme.md"));
        #[cfg(not(windows))]
        assert!(!wildcard_match("README.MD", "readme.md"));
    }
}
