//! Password authentication: HTTP Basic, `?auth_token=<base64>`, session tokens,
//! and the `Host` guard that blocks DNS rebinding.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::error::Respond;

/// Host names and addresses this server answers to.
///
/// A DNS-rebinding page keeps its own `Host` header while the connection lands
/// on loopback, so only loopback and the configured bind name are trusted.
#[derive(Debug, Clone)]
pub enum AllowedHosts {
    /// Bound to every interface (`0.0.0.0`/`::`): LAN clients may use any name.
    Any,
    /// Exact host names; loopback names and addresses are always accepted.
    List(Arc<[String]>),
}

impl AllowedHosts {
    /// Derive the accepted hosts from the `--hostname` bind address.
    #[must_use]
    pub fn from_bind_hostname(hostname: Option<&str>) -> Self {
        let mut entries = vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ];
        let Some(raw) = hostname.map(str::trim).filter(|name| !name.is_empty()) else {
            return Self::List(entries.into());
        };
        let (name, _) = split_host(raw);
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        match name.as_str() {
            // Binding all interfaces is an explicit invitation to the network;
            // rejecting arbitrary names here would break LAN hostnames.
            "0.0.0.0" | "::" => Self::Any,
            "" => Self::List(entries.into()),
            _ => {
                if !entries.contains(&name) {
                    entries.push(name);
                }
                Self::List(entries.into())
            }
        }
    }

    /// Whether a `Host` header value is safe to answer.
    #[must_use]
    pub fn permits(&self, host: &str) -> bool {
        let (name, _) = split_host(host);
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        if name.is_empty() {
            return false;
        }
        if name == "localhost" || name.ends_with(".localhost") {
            return true;
        }
        if name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
        {
            return true;
        }
        match self {
            Self::Any => true,
            Self::List(list) => list.iter().any(|allowed| allowed == &name),
        }
    }
}

/// Middleware rejecting requests whose `Host` does not name this server.
pub async fn host_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(host) = req.headers().get(HOST) {
        let allowed = match host.to_str() {
            Ok(host) => state.allowed_hosts().permits(host),
            Err(_) => false,
        };
        if !allowed {
            let error = oc_types::ApiError::new(
                StatusCode::FORBIDDEN.as_u16(),
                "ForbiddenError",
                "Host not allowed",
            );
            return error.respond();
        }
    }
    next.run(req).await
}

/// Middleware requiring a valid credential when a password is configured.
pub async fn require(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.password() else {
        return next.run(req).await;
    };

    if authorized(&req, expected, state.port()) {
        return next.run(req).await;
    }

    let mut response = oc_types::ApiError::unauthorized().respond();
    *response.status_mut() = StatusCode::UNAUTHORIZED;

    // Browsers navigating get a challenge; fetch/XHR do not (upstream behavior).
    let is_navigation = req
        .headers()
        .get("sec-fetch-mode")
        .and_then(|value| value.to_str().ok())
        .is_none_or(|mode| mode == "navigate");
    if is_navigation {
        response.headers_mut().insert(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Basic realm=\"Secure Area\""),
        );
    }
    response
}

fn authorized(req: &Request, expected: &str, port: u16) -> bool {
    if req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| basic_matches(value, expected))
    {
        return true;
    }

    if req
        .uri()
        .query()
        .and_then(|query| query_param(query, "auth_token"))
        .is_some_and(|token| token_matches(&token, expected))
    {
        return true;
    }

    port != 0 && same_origin(req) && cookie_matches(req, expected, port)
}

fn cookie_matches(req: &Request, password: &str, port: u16) -> bool {
    let name = format!("opencode_session_{port}");
    let Some(cookies) = req
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    cookies.split(';').any(|part| {
        part.trim().split_once('=').is_some_and(|(key, value)| {
            key == name && crate::session_token::verify(password, value)
        })
    })
}

/// Browsers must not be able to use a cookie minted for another origin.
fn same_origin(req: &Request) -> bool {
    let Some(origin) = req
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    let Some(host) = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let (host_name, host_port) = split_host(host);
    if !origin
        .host_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(host_name))
    {
        return false;
    }
    match (origin.port(), host_port) {
        (Some(origin_port), Some(host_port)) => origin_port == host_port,
        (Some(_), None) | (None, _) => true,
    }
}

fn split_host(host: &str) -> (&str, Option<u16>) {
    if let Some(rest) = host.strip_prefix('[')
        && let Some((name, suffix)) = rest.split_once(']')
    {
        let port = suffix.strip_prefix(':').and_then(|port| port.parse().ok());
        return (name, port);
    }
    match host.rsplit_once(':') {
        Some((name, port)) => match port.parse::<u16>() {
            Ok(port) => (name, Some(port)),
            Err(_) => (host, None),
        },
        None => (host, None),
    }
}

fn basic_matches(header: &str, expected: &str) -> bool {
    let Some(encoded) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = BASE64.decode(encoded.trim()) else {
        return false;
    };
    let Ok(pair) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, password)) = pair.split_once(':') else {
        return false;
    };
    user == "opencode" && password_matches(password, expected)
}

fn token_matches(token: &str, expected: &str) -> bool {
    // Pairing links issue session tokens; older clients send the base64 pair.
    if crate::session_token::verify(expected, token) {
        return true;
    }
    let Ok(decoded) = BASE64.decode(token) else {
        return false;
    };
    let Ok(pair) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, password)) = pair.split_once(':') else {
        return false;
    };
    user == "opencode" && password_matches(password, expected)
}

/// Session tokens are accepted wherever the password is, like upstream.
fn password_matches(candidate: &str, expected: &str) -> bool {
    constant_time_eq(candidate, expected) || crate::session_token::verify(expected, candidate)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(key)? == name {
            return percent_decode(value);
        }
    }
    None
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1])?;
                let low = hex(bytes[index + 2])?;
                out.push((high << 4) | low);
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b").as_deref(), Some("a b"));
        assert_eq!(percent_decode("a+b").as_deref(), Some("a b"));
        assert_eq!(
            percent_decode("eyJhIjoxfQ%3D%3D").as_deref(),
            Some("eyJhIjoxfQ==")
        );
        assert_eq!(percent_decode("%zz"), None);
    }

    #[test]
    fn query_param_finds_value() {
        assert_eq!(
            query_param("a=1&auth_token=abc%3D", "auth_token").as_deref(),
            Some("abc=")
        );
        assert_eq!(query_param("a=1", "auth_token"), None);
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
    }

    #[test]
    fn allowed_hosts_reject_rebinding_names() {
        let hosts = AllowedHosts::from_bind_hostname(Some("127.0.0.1"));
        for allowed in ["localhost", "localhost:4096", "127.0.0.1", "[::1]:4096"] {
            assert!(hosts.permits(allowed), "rejected {allowed}");
        }
        for rejected in ["evil.example", "localhost.evil.example", "0.0.0.0"] {
            assert!(!hosts.permits(rejected), "accepted {rejected}");
        }
    }

    #[test]
    fn wildcard_bind_accepts_any_host() {
        for bind in ["0.0.0.0", "::", "[::]"] {
            let hosts = AllowedHosts::from_bind_hostname(Some(bind));
            assert!(hosts.permits("lane.example:4096"), "rejected for {bind}");
            assert!(hosts.permits("127.0.0.1:4096"), "rejected for {bind}");
        }
    }

    #[test]
    fn configured_bind_hostname_is_accepted() {
        let hosts = AllowedHosts::from_bind_hostname(Some("workhost.local"));
        assert!(hosts.permits("workhost.local:4096"));
        assert!(!hosts.permits("other.local:4096"));
    }

    #[test]
    fn empty_password_disables_auth_at_state_construction() {
        let info = || oc_types::ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 1,
            hostname: Some("127.0.0.1".to_owned()),
            urls: Vec::new(),
            paths: oc_types::ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        };
        let empty = crate::AppState::new(info(), Some(String::new()), std::env::temp_dir());
        assert!(
            empty.password().is_none(),
            "empty password must disable auth"
        );
        let configured =
            crate::AppState::new(info(), Some("secret".to_owned()), std::env::temp_dir());
        assert_eq!(configured.password(), Some("secret"));
    }

    #[test]
    fn session_tokens_are_credentials() {
        let token = crate::session_token::issue("secret");
        assert!(token_matches(&token, "secret"));
        assert!(!token_matches(&token, "other"));
        let basic_token = format!("Basic {}", BASE64.encode(format!("opencode:{token}")));
        let basic_password = format!("Basic {}", BASE64.encode("opencode:secret"));
        assert!(basic_matches(&basic_token, "secret"));
        assert!(basic_matches(&basic_password, "secret"));
        assert!(!basic_matches(&basic_token, "other"));
        let wrong_user = format!("Basic {}", BASE64.encode(format!("other:{token}")));
        assert!(!basic_matches(&wrong_user, "secret"));
    }
}
