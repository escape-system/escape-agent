//! HMAC session tokens for the browser cookie.
//!
//! Format: `<unix_expiry>.<base64url(hmac_sha256(key, expiry))>` where the key
//! is `HMAC-SHA256(password, "opencode-session-v1")`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Cookie lifetime: 30 days.
pub const TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Build a token valid for `TTL_SECONDS`.
#[must_use]
pub fn issue(password: &str) -> String {
    issue_at(password, now_seconds() + TTL_SECONDS)
}

/// Build a token with an explicit expiry (tests and rotation).
#[must_use]
pub fn issue_at(password: &str, expiry: u64) -> String {
    let expiry = expiry.to_string();
    let signature = sign(password, expiry.as_bytes());
    format!("{expiry}.{signature}")
}

/// Verify signature and expiry.
#[must_use]
pub fn verify(password: &str, token: &str) -> bool {
    let Some((expiry, signature)) = token.split_once('.') else {
        return false;
    };
    let Ok(expiry_value) = expiry.parse::<u64>() else {
        return false;
    };
    if expiry_value <= now_seconds() {
        return false;
    }
    let expected = sign(password, expiry.as_bytes());
    expected.as_bytes().ct_eq(signature.as_bytes()).into()
}

fn sign(password: &str, payload: &[u8]) -> String {
    let mut key_mac =
        HmacSha256::new_from_slice(password.as_bytes()).expect("HMAC accepts keys of any length");
    key_mac.update(b"opencode-session-v1");
    let key = key_mac.finalize().into_bytes();

    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts keys of any length");
    mac.update(payload);
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips() {
        let token = issue("secret");
        assert!(verify("secret", &token));
        assert!(!verify("other", &token));
    }

    #[test]
    fn expired_token_is_rejected() {
        let token = issue_at("secret", now_seconds().saturating_sub(1));
        assert!(!verify("secret", &token));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        for token in ["", "x", "1.x", ".", "abc.def", "1."] {
            assert!(!verify("secret", token), "accepted {token:?}");
        }
    }
}
