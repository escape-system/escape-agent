//! Secret detector for memory writes.
//!
//! Every shape names a rule and is compiled once. Detection returns the rule,
//! never the match, so errors can be logged and shown without leaking the
//! credential the user was trying to store.

use std::sync::LazyLock;

use regex::Regex;

use crate::error::{MemoryError, SecretRule};

struct Rule {
    rule: SecretRule,
    pattern: Regex,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    [
        (
            SecretRule::PrivateKey,
            r"(?i)-----BEGIN(?: [A-Z0-9]+)* PRIVATE KEY-----",
        ),
        (SecretRule::OpenAiKey, r"(?i)\bsk-[A-Za-z0-9_-]{16,}"),
        (SecretRule::GitHubToken, r"(?i)\bghp_[A-Za-z0-9]{20,}"),
        (
            SecretRule::SlackToken,
            r"(?i)\bxox[baprs]-[A-Za-z0-9-]{10,}",
        ),
        (SecretRule::AwsAccessKey, r"(?i)\bAKIA[0-9A-Z]{16}\b"),
        (
            SecretRule::Jwt,
            r"(?i)\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        ),
        (
            SecretRule::CredentialAssignment,
            r#"(?i)\b(?:password|api[_-]?key)\s*[=:]\s*["']?[^\s"']{3,}"#,
        ),
    ]
    .into_iter()
    .map(|(rule, source)| Rule {
        rule,
        pattern: Regex::new(source).expect("static secret pattern must compile"),
    })
    .collect()
});

/// Returns the first credential rule matching `text`, if any.
#[must_use]
pub fn detect_secret(text: &str) -> Option<SecretRule> {
    RULES
        .iter()
        .find(|rule| rule.pattern.is_match(text))
        .map(|rule| rule.rule)
}

/// Rejects `text` when it contains a credential shape.
///
/// The returned error names the rule and never includes the matched text.
pub fn ensure_no_secret(text: &str) -> Result<(), MemoryError> {
    detect_secret(text).map_or(Ok(()), |rule| Err(MemoryError::Secret { rule }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const GITHUB_TOKEN: &str = "ghp_1234567890abcdefghijklmnopqrstuvwx";
    const OPENAI_KEY: &str = "sk-proj-abcdefghijklmnopqrstuvwxyz";
    const SLACK_TOKEN: &str = "xoxb-1234567890-abcdefghijklmnop";
    const JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
    const PEM_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----";

    #[test]
    fn detects_each_credential_shape() {
        let cases = [
            (OPENAI_KEY, SecretRule::OpenAiKey),
            (GITHUB_TOKEN, SecretRule::GitHubToken),
            (SLACK_TOKEN, SecretRule::SlackToken),
            (AWS_KEY, SecretRule::AwsAccessKey),
            (JWT, SecretRule::Jwt),
            (
                "aws key akiaiosfodnn7example lives here",
                SecretRule::AwsAccessKey,
            ),
            (
                "jwt eyjhbGciOiJIUzI1NiJ9.eyjzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                SecretRule::Jwt,
            ),
            (PEM_KEY, SecretRule::PrivateKey),
            (
                "the password=hunter2 for staging",
                SecretRule::CredentialAssignment,
            ),
            ("api_key=abc123", SecretRule::CredentialAssignment),
            ("API-KEY: abc123", SecretRule::CredentialAssignment),
        ];
        for (text, expected) in cases {
            assert_eq!(detect_secret(text), Some(expected), "text: {text}");
        }
    }

    #[test]
    fn accepts_ordinary_memory_text() {
        for text in [
            "Use rusqlite with a single writer; sqlx adds compile time we don't need.",
            "The scribe runs after the root turn settles.",
            "The password is stored by the OS keychain.", // no assignment value
            "Ask the user before writing a memory.",
        ] {
            assert_eq!(detect_secret(text), None, "text: {text}");
        }
    }

    #[test]
    fn rejection_never_echoes_the_secret() {
        let error = ensure_no_secret(&format!("deploy with {OPENAI_KEY} now")).unwrap_err();
        let message = error.to_string();
        assert!(!message.contains(OPENAI_KEY), "leaked secret: {message}");
        assert!(message.contains("openai-api-key"), "{message}");
        assert!(matches!(
            error,
            MemoryError::Secret {
                rule: SecretRule::OpenAiKey
            }
        ));
    }
}
