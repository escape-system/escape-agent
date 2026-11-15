//! Typed errors for catalog resolution and provider transport.
//!
//! Every error is data: no constructor in this crate panics on malformed input.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Classification of a provider failure.
///
/// Shared by the retry policy, the fallback chain and the
/// `session.model.fallback` event, so one `snake_case` spelling appears in the
/// log and in configuration (`models.fallback.on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    EmptyResponse,
    Transport,
    RateLimit,
    Quota,
    Auth,
    InvalidRequest,
    /// The provider no longer serves the requested model (retired, removed
    /// from the account, or unknown to it). Distinct from [`Self::Auth`] and
    /// [`Self::InvalidRequest`] so the caller can advance the model chain.
    ModelUnavailable,
    #[default]
    Unknown,
}

impl FailureKind {
    /// Wire/config string for this failure class.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmptyResponse => "empty_response",
            Self::Transport => "transport",
            Self::RateLimit => "rate_limit",
            Self::Quota => "quota",
            Self::Auth => "auth",
            Self::InvalidRequest => "invalid_request",
            Self::ModelUnavailable => "model_unavailable",
            Self::Unknown => "unknown",
        }
    }

    /// True when the same request may succeed later without changing it.
    /// `Auth` and `InvalidRequest` are permanent; `Unknown` is not guessed at.
    ///
    /// `ModelUnavailable` is deliberately not retryable: the identical request
    /// cannot make a retired model exist again, so a transport retry only burns
    /// quota and delays the inevitable. Chain advancement is a separate
    /// mechanism — `oc-runtime`'s `FallbackPolicy` special-cases this kind and
    /// moves to the next candidate regardless of `models.fallback.on`.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::EmptyResponse | Self::Transport | Self::RateLimit | Self::Quota
        )
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Substring rules, in evaluation order: the first group with any matching
/// entry decides the kind, so earlier groups win over later ones.
const MESSAGE_RULES: [(&[&str], FailureKind); 5] = [
    (
        &[
            "no longer available",
            "not available",
            "model not found",
            "unknown model",
            "does not exist",
            "has been deprecated",
            "decommissioned",
            "retired",
            "not supported for this account",
        ],
        FailureKind::ModelUnavailable,
    ),
    (
        &[
            "resource has been exhausted",
            "resource_exhausted",
            "quota",
            "insufficient_quota",
            "out of credits",
            "credit balance",
            "billing",
            // Every Antigravity account is cooling down: the turn has no live
            // account for this model, which the chain recovers from exactly like
            // a quota window (the gateway only emits this once every eligible
            // account failed this request). Without this needle the sentence is
            // `Unknown`, which the default `models.fallback.on` does not list, so
            // the turn died on the only model it tried.
            "cooling down",
        ],
        FailureKind::Quota,
    ),
    (
        &[
            "rate limit",
            "rate_limit",
            "too many requests",
            "overloaded",
        ],
        FailureKind::RateLimit,
    ),
    (
        &[
            "invalid api key",
            "unauthorized",
            "authentication",
            "permission denied",
        ],
        FailureKind::Auth,
    ),
    (
        &["context length", "invalid_request", "max tokens"],
        FailureKind::InvalidRequest,
    ),
];

/// Classify a provider error message into a [`FailureKind`].
///
/// Pure and total: the message is lowercased and matched against
/// [`MESSAGE_RULES`] in order, and an unrecognized message is
/// [`FailureKind::Unknown`]. Callers use this to turn an opaque provider
/// string into a class the fallback chain can act on; the message text itself
/// is preserved by the caller.
///
/// Ordering matters: "Resource has been exhausted (e.g. check quota)." is a
/// quota failure, not a rate limit, and "Claude Opus 4.6 is no longer
/// available." is a retired model, not a transient one.
#[must_use]
pub fn classify_provider_message(message: &str) -> FailureKind {
    let lower = message.to_ascii_lowercase();
    for (needles, kind) in MESSAGE_RULES {
        if needles.iter().any(|needle| lower.contains(needle)) {
            return kind;
        }
    }
    FailureKind::Unknown
}

/// Error returned by catalog loading, model resolution, and provider calls.
#[derive(Debug)]
pub enum LlmError {
    InvalidReference {
        reference: String,
    },
    Catalog {
        message: String,
    },
    UnknownProvider {
        provider: String,
        candidates: Vec<String>,
    },
    UnknownModel {
        provider: String,
        model: String,
        candidates: Vec<String>,
    },
    UnknownVariant {
        provider: String,
        model: String,
        variant: String,
        available: Vec<String>,
    },
    UnsupportedProtocol {
        provider: String,
        model: String,
        protocol: String,
    },
    InvalidRequest {
        message: String,
    },
    Transport {
        message: String,
    },
    Http {
        status: u16,
        message: String,
        retry_after_ms: Option<u64>,
    },
    Timeout {
        phase: &'static str,
    },
    Provider {
        message: String,
    },
    /// A provider stream failed in a classifiable way (empty output, transport,
    /// rate limit, quota, auth, invalid request); additive over the variants
    /// above, whose meaning is unchanged.
    Failure {
        kind: FailureKind,
        message: String,
    },
    Config {
        message: String,
    },
}

fn candidates_suffix(candidates: &[String]) -> String {
    if candidates.is_empty() {
        String::new()
    } else {
        format!("; candidates: {}", candidates.join(", "))
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LlmError::InvalidReference { reference } => write!(
                f,
                "invalid model reference `{reference}`: expected `provider/id[#variant]`"
            ),
            LlmError::Catalog { message } => write!(f, "invalid catalog: {message}"),
            LlmError::UnknownProvider {
                provider,
                candidates,
            } => write!(
                f,
                "unknown provider `{provider}`{}",
                candidates_suffix(candidates)
            ),
            LlmError::UnknownModel {
                provider,
                model,
                candidates,
            } => write!(
                f,
                "unknown model `{model}` for provider `{provider}`{}",
                candidates_suffix(candidates)
            ),
            LlmError::UnknownVariant {
                provider,
                model,
                variant,
                available,
            } => {
                let available = if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                };
                write!(
                    f,
                    "unknown variant `{variant}` for `{provider}/{model}`; available: {available}"
                )
            }
            LlmError::UnsupportedProtocol {
                provider,
                model,
                protocol,
            } => write!(
                f,
                "model `{provider}/{model}` does not support the {protocol} protocol"
            ),
            LlmError::InvalidRequest { message } => write!(f, "invalid request: {message}"),
            LlmError::Transport { message } => write!(f, "transport error: {message}"),
            LlmError::Http {
                status,
                message,
                retry_after_ms,
            } => match retry_after_ms {
                Some(delay) => write!(
                    f,
                    "provider returned HTTP {status}: {message} (retry after {delay} ms)"
                ),
                None => write!(f, "provider returned HTTP {status}: {message}"),
            },
            LlmError::Timeout { phase } => write!(f, "timed out while {phase}"),
            LlmError::Provider { message } => write!(f, "provider stream error: {message}"),
            LlmError::Failure { kind, message } => {
                write!(f, "provider failure ({kind}): {message}")
            }
            LlmError::Config { message } => {
                write!(f, "catalog configuration is invalid: {message}")
            }
        }
    }
}

impl std::error::Error for LlmError {}

impl LlmError {
    /// True when retrying the same request may succeed without changing it.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::Http {
                status,
                message,
                retry_after_ms,
            } => {
                if *status == 429 {
                    if retry_after_ms.is_some_and(|ms| ms > 60_000) {
                        return false;
                    }
                    let lower = message.to_ascii_lowercase();
                    if lower.contains("usage limit")
                        || lower.contains("quota")
                        || lower.contains("credit")
                        || lower.contains("balance")
                        || lower.contains("exceeded")
                    {
                        return false;
                    }
                }
                matches!(status, 408 | 409 | 425 | 429) || (500..=599).contains(status)
            }
            LlmError::Transport { .. } => true,
            LlmError::Failure { kind, .. } => kind.is_retryable(),
            LlmError::Timeout { .. }
            | LlmError::Provider { .. }
            | LlmError::InvalidRequest { .. }
            | LlmError::InvalidReference { .. }
            | LlmError::Catalog { .. }
            | LlmError::UnknownProvider { .. }
            | LlmError::UnknownModel { .. }
            | LlmError::UnknownVariant { .. }
            | LlmError::UnsupportedProtocol { .. }
            | LlmError::Config { .. } => false,
        }
    }

    /// Milliseconds the provider asked the caller to wait, if any.
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            LlmError::Http { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }
}

impl From<reqwest::Error> for LlmError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            LlmError::Timeout {
                phase: "waiting for the provider",
            }
        } else {
            LlmError::Transport {
                message: error.to_string(),
            }
        }
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(error: serde_json::Error) -> Self {
        LlmError::Catalog {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn failure_kind_round_trips_and_classifies_retries() {
        for (kind, wire, retryable) in [
            (FailureKind::EmptyResponse, "empty_response", true),
            (FailureKind::Transport, "transport", true),
            (FailureKind::RateLimit, "rate_limit", true),
            (FailureKind::Quota, "quota", true),
            (FailureKind::Auth, "auth", false),
            (FailureKind::InvalidRequest, "invalid_request", false),
            (FailureKind::ModelUnavailable, "model_unavailable", false),
            (FailureKind::Unknown, "unknown", false),
        ] {
            assert_eq!(kind.as_str(), wire);
            assert_eq!(kind.is_retryable(), retryable, "{wire}");
            assert_eq!(serde_json::to_value(kind).expect("serialize"), json!(wire));
            let parsed: FailureKind = serde_json::from_value(json!(wire)).expect("deserialize");
            assert_eq!(parsed, kind);
            assert_eq!(parsed.to_string(), wire);
        }
        assert_eq!(FailureKind::default(), FailureKind::Unknown);
    }

    #[test]
    fn failure_variant_drives_is_retryable() {
        let quota = LlmError::Failure {
            kind: FailureKind::Quota,
            message: "out of credit".to_owned(),
        };
        assert!(quota.is_retryable());
        let auth = LlmError::Failure {
            kind: FailureKind::Auth,
            message: "invalid api key".to_owned(),
        };
        assert!(!auth.is_retryable());
        assert!(auth.to_string().contains("auth"));
        assert_eq!(auth.retry_after_ms(), None);
        assert!(
            LlmError::Transport {
                message: "reset".to_owned()
            }
            .is_retryable()
        );
        assert!(
            !LlmError::InvalidRequest {
                message: "bad".to_owned()
            }
            .is_retryable()
        );
    }

    #[test]
    fn classifier_recognizes_a_retired_model_before_its_quota_hint() {
        assert_eq!(
            classify_provider_message("Claude Opus 4.6 is no longer available."),
            FailureKind::ModelUnavailable
        );
        assert_eq!(
            classify_provider_message("The model `gpt-4-turbo` has been deprecated"),
            FailureKind::ModelUnavailable
        );
        assert_eq!(
            classify_provider_message("MODEL_NOT_FOUND: unknown model `foo`"),
            FailureKind::ModelUnavailable
        );
        assert_eq!(
            classify_provider_message("this model is not supported for this account"),
            FailureKind::ModelUnavailable
        );
    }

    #[test]
    fn classifier_separates_quota_from_rate_limit() {
        assert_eq!(
            classify_provider_message("Resource has been exhausted (e.g. check quota)."),
            FailureKind::Quota
        );
        assert_eq!(
            classify_provider_message("insufficient_quota: billing hard limit reached"),
            FailureKind::Quota
        );
        assert_eq!(
            classify_provider_message("429 Too Many Requests: rate limit reached"),
            FailureKind::RateLimit
        );
        assert_eq!(
            classify_provider_message("provider is overloaded, try again"),
            FailureKind::RateLimit
        );
    }

    #[test]
    fn classifier_covers_auth_and_unknown_messages() {
        assert_eq!(
            classify_provider_message("401 Unauthorized: invalid api key"),
            FailureKind::Auth
        );
        assert_eq!(
            classify_provider_message("permission denied for this key"),
            FailureKind::Auth
        );
        assert_eq!(
            classify_provider_message("maximum context length exceeded"),
            FailureKind::InvalidRequest
        );
        assert_eq!(
            classify_provider_message("the server exploded"),
            FailureKind::Unknown
        );
    }

    #[test]
    fn classifier_treats_an_all_accounts_cooling_message_as_quota() {
        // The exact sentence shape `oc-antigravity`'s pool emits once every
        // eligible account failed this request. The chain must advance, so this
        // has to be `Quota` and not `Unknown`.
        assert_eq!(
            classify_provider_message(
                "all Antigravity accounts are cooling down until 1763260800000 \
                 (every eligible account already failed this request)"
            ),
            FailureKind::Quota
        );
        assert_eq!(
            classify_provider_message(
                "all Antigravity accounts are cooling down until 1763260800000 \
                 (next reset in 4200s, exceeds the per-turn wait budget)"
            ),
            FailureKind::Quota
        );
        // A sentence with no rule still maps to `Unknown`, so the needle above
        // is the reason this classifies and not a broad catch-all.
        assert_eq!(
            classify_provider_message("the seas were angry that day"),
            FailureKind::Unknown
        );
    }

    #[test]
    fn model_unavailable_is_never_transport_retryable() {
        let error = LlmError::Failure {
            kind: FailureKind::ModelUnavailable,
            message: "Claude Opus 4.6 is no longer available.".to_owned(),
        };
        assert!(
            !error.is_retryable(),
            "a retry cannot resurrect a retired model; the chain advances instead"
        );
        assert!(error.to_string().contains("model_unavailable"));
        assert_eq!(
            serde_json::to_value(FailureKind::ModelUnavailable).expect("serialize"),
            json!("model_unavailable")
        );
    }
}
