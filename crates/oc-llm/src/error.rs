//! Typed errors for catalog resolution and provider transport.
//!
//! Every error is data: no constructor in this crate panics on malformed input.

use std::fmt;

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
            LlmError::Http { status, .. } => {
                matches!(status, 408 | 409 | 425 | 429) || (500..=599).contains(status)
            }
            LlmError::Transport { .. } => true,
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
