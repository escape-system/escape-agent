//! Manual redirect loop and the bounded body stream.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{Client, StatusCode};

use crate::ToolError;

use super::guard::{Target, dns_pins, validate_target, verify_remote};
use super::{BodyBuffer, Format, MAX_REDIRECTS};

const USER_AGENT: &str = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; OpenCode-User/1.0; +https://opencode.ai";

pub(super) struct Fetched {
    pub(super) status: u16,
    pub(super) content_type: String,
    pub(super) body: BodyBuffer,
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

pub(super) async fn fetch(
    timeout: Duration,
    mut target: Target,
    format: Format,
) -> Result<Fetched, ToolError> {
    let mut redirects = 0_usize;
    loop {
        let client = build_client(timeout, &target)?;
        let request = client
            .get(target.url.clone())
            .header(ACCEPT, format.accept())
            .send()
            .await
            .map_err(|error| ToolError::new(format!("Unable to fetch {}: {error}", target.url)))?;
        verify_remote(&target, &request)?;

        let status = request.status();
        let location = if is_redirect(status) {
            request.headers().get(LOCATION).cloned()
        } else {
            None
        };
        if let Some(location) = location {
            let location = location.to_str().map_err(|_| {
                ToolError::new(format!(
                    "Invalid redirect target from {}: non-ASCII location",
                    target.url
                ))
            })?;
            if redirects >= MAX_REDIRECTS {
                return Err(ToolError::new(format!(
                    "Too many redirects (more than {MAX_REDIRECTS}): {}",
                    target.url
                )));
            }
            let next_url = target.url.join(location).map_err(|error| {
                ToolError::new(format!(
                    "Invalid redirect target from {}: {error}",
                    target.url
                ))
            })?;
            target = validate_target(&next_url, next_url.as_str()).await?;
            redirects += 1;
            continue;
        }

        let content_type = request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let mut response = request;
        let mut body = BodyBuffer::new();
        loop {
            let chunk = response.chunk().await.map_err(|error| {
                ToolError::new(format!(
                    "Unable to read response from {}: {error}",
                    target.url
                ))
            })?;
            let Some(chunk) = chunk else { break };
            body.push(&chunk);
            if body.truncated() {
                break;
            }
        }

        return Ok(Fetched {
            status: status.as_u16(),
            content_type,
            body,
        });
    }
}

pub(super) fn build_client(timeout: Duration, target: &Target) -> Result<Client, ToolError> {
    let mut builder = Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(USER_AGENT);
    if let Some((domain, pins)) = dns_pins(target) {
        builder = builder.resolve_to_addrs(domain, &pins);
    }
    builder
        .build()
        .map_err(|error| ToolError::new(format!("Unable to create HTTP client: {error}")))
}
