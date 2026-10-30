//! Typed failures for memory retrieval, writes and the scribe.

use oc_store::StoreError;
use thiserror::Error;

/// The credential shape that caused a memory write to be rejected.
///
/// The rule names the pattern, never the matched text: error messages are
/// logged, surfaced to the model and shown in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretRule {
    /// `sk-…` provider API key.
    OpenAiKey,
    /// `ghp_…` GitHub personal access token.
    GitHubToken,
    /// `xox[baprs]-…` Slack token.
    SlackToken,
    /// `AKIA…` AWS access key id.
    AwsAccessKey,
    /// `eyJ….….…` JSON Web Token.
    Jwt,
    /// `-----BEGIN … PRIVATE KEY-----` PEM block.
    PrivateKey,
    /// `password=…` / `api_key=…` assignment with a value.
    CredentialAssignment,
}

impl SecretRule {
    /// Stable machine-readable rule name.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenAiKey => "openai-api-key",
            Self::GitHubToken => "github-token",
            Self::SlackToken => "slack-token",
            Self::AwsAccessKey => "aws-access-key",
            Self::Jwt => "json-web-token",
            Self::PrivateKey => "private-key",
            Self::CredentialAssignment => "credential-assignment",
        }
    }
}

impl std::fmt::Display for SecretRule {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id())
    }
}

/// Failure from retrieval, a memory write, a tool call or the scribe.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// SQLite rejected a statement or the row could not be decoded.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The text matches a credential shape. The message names the rule only.
    #[error(
        "memory rejected: the text contains what looks like a {rule}; remove it and write again"
    )]
    Secret {
        /// Detector rule that matched.
        rule: SecretRule,
    },

    /// A caller-supplied value is outside the memory contract.
    #[error("invalid memory input: {0}")]
    Invalid(String),

    /// The text is longer than [`crate::service::TEXT_CAP`].
    #[error(
        "memory text is {chars} characters; the cap is {cap} characters — put longer notes in a file"
    )]
    TextTooLong {
        /// Character count of the submitted text.
        chars: usize,
        /// Maximum accepted character count.
        cap: usize,
    },

    /// The scribe model did not return a JSON array of candidates.
    #[error("memory model output is not a usable JSON array: {0}")]
    ModelJson(String),

    /// The scribe model call itself failed.
    #[error("memory model call failed: {0}")]
    Model(String),
}

impl MemoryError {
    /// Convenience constructor for [`MemoryError::Invalid`].
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_error_display_names_the_rule() {
        let error = MemoryError::Secret {
            rule: SecretRule::OpenAiKey,
        };
        let message = error.to_string();
        assert!(message.contains("openai-api-key"), "{message}");
        assert!(message.contains("memory rejected"), "{message}");
    }
}
