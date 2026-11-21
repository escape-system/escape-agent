use reqwest::Client;
use serde_json::Value;

use super::PROBE_TIMEOUT;

mod deepseek;
mod minimax;
mod moonshot;
mod opencode_go;
mod openrouter;
mod siliconflow;
mod zai;

pub(super) use deepseek::probe_deepseek;
pub(super) use minimax::probe_minimax;
pub(super) use moonshot::probe_moonshot;
pub(super) use opencode_go::probe_opencode_go;
pub(super) use openrouter::probe_openrouter;
pub(super) use siliconflow::probe_siliconflow;
pub(super) use zai::probe_zai;

/// Maximum number of upstream error-body characters kept in a probe error.
const ERROR_EXCERPT_MAX_CHARS: usize = 120;

/// Token prefixes that must never reach the cached, UI-visible probe error.
/// `oc_sk_` is the credential format used by `opencode-go`; a bare token has
/// no `sk-` substring, so it needs its own entry.
const SECRET_PREFIXES: [&str; 5] = ["oc_sk_", "sk-", "AIza", "Bearer ", "bearer "];

async fn get_json(http: &Client, url: &str, bearer_key: &str) -> Result<Value, String> {
    let response = http
        .get(url)
        .bearer_auth(bearer_key)
        .header(
            reqwest::header::USER_AGENT,
            concat!("opencode/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("failed to read response body: {err}"))?;
    if !status.is_success() {
        let endpoint = endpoint_path(url);
        let excerpt = sanitize_error_excerpt(&text);
        return Err(if excerpt.is_empty() {
            format!("HTTP {} at {endpoint}", status.as_u16())
        } else {
            format!("HTTP {} at {endpoint}: {excerpt}", status.as_u16())
        });
    }
    serde_json::from_str(&text).map_err(|err| format!("invalid JSON: {err}"))
}

/// Endpoint path of a probe URL; the full URL (host, query) is never surfaced.
fn endpoint_path(url: &str) -> String {
    reqwest::Url::parse(url).ok().map_or_else(
        || "/".to_owned(),
        |parsed| {
            let path = parsed.path();
            if path.is_empty() {
                "/".to_owned()
            } else {
                path.to_owned()
            }
        },
    )
}

/// Bound an upstream error body and strip anything that looks like a bearer token.
fn sanitize_error_excerpt(text: &str) -> String {
    let redacted = redact_secrets(text);
    let mut excerpt: String = redacted.chars().take(ERROR_EXCERPT_MAX_CHARS).collect();
    if redacted.chars().count() > ERROR_EXCERPT_MAX_CHARS {
        excerpt.push('\u{2026}');
    }
    excerpt.trim().to_owned()
}

fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = SECRET_PREFIXES
        .iter()
        .filter_map(|prefix| rest.find(prefix))
        .min()
    {
        out.push_str(&rest[..start]);
        let secret = &rest[start..];
        out.push_str("[redacted]");
        rest = &secret[secret_token_len(secret)..];
    }
    out.push_str(rest);
    out
}

/// Byte length of a credential starting at the beginning of `text`.
fn secret_token_len(text: &str) -> usize {
    let prefix_len = SECRET_PREFIXES
        .iter()
        .find(|prefix| text.starts_with(**prefix))
        .map_or(0, |prefix| prefix.len());
    let mut len = prefix_len;
    for ch in text[prefix_len..].chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+' | '/' | '=') {
            len += ch.len_utf8();
        } else {
            break;
        }
    }
    len
}

#[cfg(test)]
mod tests {
    use super::sanitize_error_excerpt;

    #[test]
    fn error_excerpts_redact_secrets_and_cap_the_excerpt() {
        let body = r#"{"error":"invalid key sk-or-secret-1234567890","token":"Bearer abc.def"}"#;
        let excerpt = sanitize_error_excerpt(body);
        assert!(!excerpt.contains("sk-or-secret"), "{excerpt}");
        assert!(!excerpt.contains("abc.def"), "{excerpt}");
        assert!(excerpt.contains("[redacted]"), "{excerpt}");
        assert!(!sanitize_error_excerpt("AIzaSyTopSecretValue").contains("AIzaSyTopSecretValue"));
        let long = "x".repeat(400);
        assert_eq!(
            sanitize_error_excerpt(&long).chars().count(),
            super::ERROR_EXCERPT_MAX_CHARS + 1
        );
    }
}
