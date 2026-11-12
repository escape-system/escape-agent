//! `webfetch` tool: bounded HTTP(S) GET with SSRF guards.
//!
//! Only `http` and `https` URLs are accepted, URLs carrying embedded
//! credentials are refused, and every hop (initial request plus each
//! redirect target) is validated before a connection is made: literal IPs and
//! DNS answers that fall into loopback, private, link-local, multicast,
//! unspecified, carrier-grade-NAT, documentation or cloud-metadata ranges are
//! rejected. Redirects are followed manually, capped at five hops, with the
//! same validation applied to each target. Bodies are capped at 5 MiB and the
//! whole fetch is bounded by a timeout (default 30s, maximum 120s).
//!
//! Formats:
//! - `text` strips HTML tags, scripts and styles, decodes entities and
//!   collapses whitespace.
//! - `markdown` is a conservative plain-text fallback built on the same
//!   renderer: headings gain `#` prefixes and list items gain `- ` bullets,
//!   but links, emphasis and tables are flattened to their text. No markdown
//!   dependency is used.
//! - `html` returns the document body exactly as received (after UTF-8
//!   decoding), limited to the 5 MiB cap.
//!
//! DNS is resolved once per hop and the vetted addresses are pinned into the
//! `reqwest` client, so a hostile resolver cannot rebind between the check and
//! the connection. The connected peer address is also verified against the
//! vetted set when the transport reports it.
//!
//! The facade holds the tool, its constants and [`BodyBuffer`]; address vetting
//! lives in [`guard`], the redirect loop and body stream in [`fetch`], and HTML
//! rendering in [`extract`].

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

mod extract;
mod fetch;
mod guard;

pub use extract::{html_to_markdown, html_to_text};
pub use guard::is_blocked_ip;

/// Maximum number of redirects followed for a single fetch.
pub const MAX_REDIRECTS: usize = 5;
/// Maximum number of body bytes retained from a response.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
/// Upper bound for the request timeout in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 120;

/// Content format requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Text,
    Markdown,
    Html,
}

impl Format {
    fn parse(value: Option<&str>) -> Result<Self, ToolError> {
        match value.unwrap_or("markdown") {
            "text" => Ok(Self::Text),
            "markdown" => Ok(Self::Markdown),
            "html" => Ok(Self::Html),
            other => Err(ToolError::new(format!(
                "Invalid arguments: `format` must be one of text, markdown, html (got `{other}`)"
            ))),
        }
    }

    fn accept(self) -> &'static str {
        match self {
            Self::Markdown => {
                "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
            }
            Self::Text => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
            Self::Html => {
                "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
            }
        }
    }
}

/// Bounded body accumulator used while streaming a response.
#[derive(Debug, Default)]
pub struct BodyBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BodyBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk, keeping at most [`MAX_BODY_BYTES`] bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.truncated {
            return;
        }
        let remaining = MAX_BODY_BYTES - self.bytes.len();
        if chunk.len() > remaining {
            self.bytes.extend_from_slice(&chunk[..remaining]);
            self.truncated = true;
        } else {
            self.bytes.extend_from_slice(chunk);
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// `webfetch` tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebfetchTool;

#[async_trait]
impl Tool for WebfetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }

    fn description(&self) -> &str {
        "Fetch content from an HTTP or HTTPS URL and return it as text, markdown, or HTML. Markdown is the default. This tool is read-only. Only http and https URLs are accepted; URLs with embedded credentials are refused, redirects are limited to five and every hop is revalidated, and hosts resolving to loopback, private, link-local, multicast, or cloud-metadata addresses are blocked. Responses are capped at 5 MiB. Markdown output is a conservative plain-text fallback (headings and list items are prefixed; links, emphasis, and tables are flattened). Timeout defaults to 30 seconds and is capped at 120."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The HTTP or HTTPS URL to fetch content from"
                },
                "format": {
                    "type": "string",
                    "enum": ["text", "markdown", "html"],
                    "description": "The format to return the content in. Defaults to markdown. html returns the document body unchanged."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_SECONDS,
                    "description": "Optional timeout in seconds (default 30, maximum 120)"
                }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let raw_url = util::required_string(&input, "url")?;
        let format = Format::parse(util::optional_string(&input, "format")?.as_deref())?;
        let timeout = timeout_from(util::optional_u64(&input, "timeout")?)?;

        let parsed = url::Url::parse(&raw_url)
            .map_err(|error| ToolError::new(format!("Invalid URL `{raw_url}`: {error}")))?;
        let target = tokio::time::timeout(timeout, guard::validate_target(&parsed, &raw_url))
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "Request timed out after {}s: {raw_url}",
                    timeout.as_secs()
                ))
            })??;

        let fetched = tokio::time::timeout(timeout, fetch::fetch(timeout, target, format))
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "Request timed out after {}s: {raw_url}",
                    timeout.as_secs()
                ))
            })??;

        let decoded = String::from_utf8_lossy(fetched.body.bytes()).into_owned();
        let is_html = fetched.content_type.to_ascii_lowercase().contains("html");
        let output = match format {
            Format::Html => decoded,
            // Markdown without an HTML parser dependency degrades to the same
            // conservative text rendering, with lightweight structural markers.
            Format::Text => {
                if is_html {
                    html_to_text(&decoded)
                } else {
                    decoded
                }
            }
            Format::Markdown => {
                if is_html {
                    html_to_markdown(&decoded)
                } else {
                    decoded
                }
            }
        };

        let metadata = json!({
            "url": raw_url,
            "status": fetched.status,
            "contentType": fetched.content_type,
            "bytes": fetched.body.len(),
            "truncated": fetched.body.truncated(),
        });
        Ok(ToolResult::new(Some(output), None, Some(metadata)))
    }
}

fn timeout_from(value: Option<u64>) -> Result<Duration, ToolError> {
    let seconds = value.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if seconds == 0 || seconds > MAX_TIMEOUT_SECONDS {
        return Err(ToolError::new(format!(
            "Invalid arguments: `timeout` must be between 1 and {MAX_TIMEOUT_SECONDS} seconds"
        )));
    }
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_buffer_caps_at_limit() {
        let mut buffer = BodyBuffer::new();
        let chunk = vec![b'a'; MAX_BODY_BYTES - 1];
        buffer.push(&chunk);
        assert_eq!(buffer.len(), MAX_BODY_BYTES - 1);
        assert!(!buffer.truncated());
        buffer.push(b"xyz");
        assert_eq!(buffer.len(), MAX_BODY_BYTES);
        assert!(buffer.truncated());
        buffer.push(b"more");
        assert_eq!(buffer.len(), MAX_BODY_BYTES);
    }
}
