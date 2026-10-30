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
//! `reqwest` client, so a hostile resolver cannot win a rebinding race between
//! the check and the connection. The connected peer address is also verified
//! against the vetted set when the transport reports it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::net::lookup_host;
use url::{Host, Url};

use crate::util;
use crate::{Tool, ToolContext, ToolError, ToolResult};

/// Maximum number of redirects followed for a single fetch.
pub const MAX_REDIRECTS: usize = 5;
/// Maximum number of body bytes retained from a response.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
/// Upper bound for the request timeout in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 120;

const USER_AGENT: &str = "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; OpenCode-User/1.0; +https://opencode.ai";

const SKIP_TAGS: [&str; 8] = [
    "script", "style", "noscript", "iframe", "object", "embed", "template", "svg",
];

const BLOCK_TAGS: [&str; 33] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

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

        let parsed = Url::parse(&raw_url)
            .map_err(|error| ToolError::new(format!("Invalid URL `{raw_url}`: {error}")))?;
        let target = tokio::time::timeout(timeout, validate_target(&parsed, &raw_url))
            .await
            .map_err(|_| {
                ToolError::new(format!(
                    "Request timed out after {}s: {raw_url}",
                    timeout.as_secs()
                ))
            })??;

        let fetched = tokio::time::timeout(timeout, fetch(timeout, target, format))
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

struct Fetched {
    status: u16,
    content_type: String,
    body: BodyBuffer,
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

async fn fetch(
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

/// A request target whose addresses have been checked and are pinned into the
/// HTTP client, so the connection cannot be redirected by a second DNS answer.
#[derive(Debug)]
struct Target {
    url: Url,
    /// Lowercase DNS name to pin; `None` for literal IP hosts.
    domain: Option<String>,
    port: u16,
    /// Validated addresses; at least one. Deduplicated and sorted.
    addresses: Vec<IpAddr>,
}

/// Validate scheme, credentials, and host, resolving DNS once and returning
/// the vetted addresses for pinning.
async fn validate_target(url: &Url, display: &str) -> Result<Target, ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::new(format!(
            "URL must use http:// or https://: {display}"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::new(format!(
            "URL must not include credentials: {display}"
        )));
    }
    let host = url
        .host()
        .ok_or_else(|| ToolError::new(format!("URL has no host: {display}")))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let (domain, mut addresses) = match host {
        Host::Ipv4(address) => (None, vec![IpAddr::V4(address)]),
        Host::Ipv6(address) => (None, vec![IpAddr::V6(address)]),
        Host::Domain(domain) => {
            let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
            if normalized == "localhost" || normalized.ends_with(".localhost") {
                return Err(blocked(display, "localhost"));
            }
            let mut addresses = Vec::new();
            let resolved = lookup_host((normalized.as_str(), port))
                .await
                .map_err(|error| {
                    ToolError::new(format!("Unable to resolve host for {display}: {error}"))
                })?;
            for address in resolved {
                addresses.push(address.ip());
            }
            (Some(normalized), addresses)
        }
    };
    if addresses.is_empty() {
        return Err(ToolError::new(format!(
            "Unable to resolve host for {display}: no addresses"
        )));
    }
    addresses.sort_unstable();
    addresses.dedup();
    for address in &addresses {
        check_ip(*address, display)?;
    }
    Ok(Target {
        url: url.clone(),
        domain,
        port,
        addresses,
    })
}

/// DNS pins to install on the client builder: the lowercase domain and every
/// vetted address at the request port.
fn dns_pins(target: &Target) -> Option<(&str, Vec<SocketAddr>)> {
    let domain = target.domain.as_deref()?;
    let pins = target
        .addresses
        .iter()
        .map(|address| SocketAddr::new(*address, target.port))
        .collect();
    Some((domain, pins))
}

fn build_client(timeout: Duration, target: &Target) -> Result<Client, ToolError> {
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

/// Reject a connection that landed on an address other than a vetted one.
fn verify_remote(target: &Target, response: &reqwest::Response) -> Result<(), ToolError> {
    let Some(remote) = response.remote_addr() else {
        return Ok(());
    };
    if target.addresses.contains(&remote.ip()) {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "Refusing to read {}: connected to unexpected address {remote}",
        target.url
    )))
}

fn check_ip(ip: IpAddr, display: &str) -> Result<(), ToolError> {
    if is_blocked_ip(ip) {
        return Err(blocked(display, &ip.to_string()));
    }
    Ok(())
}

fn blocked(display: &str, host: &str) -> ToolError {
    ToolError::new(format!(
        "Refusing to fetch {display}: host `{host}` is blocked (loopback, private, link-local, multicast, metadata, or otherwise non-public)"
    ))
}

/// Whether an address must never be reached by `webfetch`.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_blocked_v4(address),
        IpAddr::V6(address) => is_blocked_v6(address),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        // 0.0.0.0/8 "this network"
        || first == 0
        // 100.64.0.0/10 carrier-grade NAT
        || (first == 100 && (64..=127).contains(&second))
        // 192.0.0.0/24 IETF protocol assignments
        || (first == 192 && second == 0 && third == 0)
        // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 documentation
        || (first == 192 && second == 0 && third == 2)
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        // 192.88.99.0/24 6to4 relay anycast
        || (first == 192 && second == 88 && third == 99)
        // 198.18.0.0/15 benchmarking
        || (first == 198 && (second == 18 || second == 19))
        // 240.0.0.0/4 reserved
        || first >= 240
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return true;
    }
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_blocked_v4(mapped);
    }
    let segments = ip.segments();
    // IPv4-compatible `::a.b.c.d`
    if segments[..6].iter().all(|segment| *segment == 0) {
        return is_blocked_v4(embedded_ipv4(segments[6], segments[7]));
    }
    // fc00::/7 unique local
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fec0::/10 deprecated site-local
    if (segments[0] & 0xffc0) == 0xfec0 {
        return true;
    }
    // fe80::/10 link-local
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // 2002::/16 6to4 and 2001::/32 Teredo tunnel arbitrary IPv4 payloads.
    if segments[0] == 0x2002 || (segments[0] == 0x2001 && segments[1] == 0) {
        return true;
    }
    // 2001:db8::/32 documentation
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return true;
    }
    // 64:ff9b::/96 NAT64 embeds an IPv4 address.
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|segment| *segment == 0)
    {
        return is_blocked_v4(embedded_ipv4(segments[6], segments[7]));
    }
    // 100::/64 discard-only
    segments[0] == 0x0100 && segments[1..4].iter().all(|segment| *segment == 0)
}

fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    let [first, second] = high.to_be_bytes();
    let [third, fourth] = low.to_be_bytes();
    Ipv4Addr::new(first, second, third, fourth)
}

/// Strip HTML to readable text: tags, scripts and styles removed, entities
/// decoded, whitespace collapsed, and block elements split across lines.
pub fn html_to_text(html: &str) -> String {
    render_html(html, Render::Text)
}

/// Conservative markdown fallback: the same text rendering with `#` heading
/// prefixes and `- ` list bullets. Links, emphasis, and tables are flattened.
pub fn html_to_markdown(html: &str) -> String {
    render_html(html, Render::Markdown)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Render {
    Text,
    Markdown,
}

fn render_html(html: &str, mode: Render) -> String {
    let html = strip_comments(html);
    let mut output = String::new();
    let mut skip_depth = 0_usize;
    let mut rest = html.as_str();
    while !rest.is_empty() {
        if !rest.starts_with('<') {
            let Some(character) = rest.chars().next() else {
                break;
            };
            if character == '&'
                && let Some((decoded, consumed)) = decode_entity(rest)
            {
                if skip_depth == 0 {
                    output.push(decoded);
                }
                rest = &rest[consumed..];
                continue;
            }
            if skip_depth == 0 {
                output.push(character);
            }
            rest = &rest[character.len_utf8()..];
            continue;
        }

        let Some(end) = rest.find('>') else {
            break;
        };
        let inner = &rest[1..end];
        rest = &rest[end + 1..];
        if inner.starts_with('!') || inner.starts_with('?') || inner.starts_with("!--") {
            continue;
        }
        let (closing, name, self_closing) = parse_tag(inner);
        if name.is_empty() {
            continue;
        }
        let skip_tag = SKIP_TAGS.contains(&name.as_str());
        if skip_depth > 0 {
            if skip_tag {
                if closing {
                    skip_depth -= 1;
                } else if !self_closing {
                    skip_depth += 1;
                }
            }
            if skip_depth == 0 {
                push_break(&mut output);
            }
            continue;
        }
        if skip_tag {
            if !closing && !self_closing {
                skip_depth = 1;
            }
            continue;
        }
        if !BLOCK_TAGS.contains(&name.as_str()) {
            continue;
        }
        if !closing && mode == Render::Markdown {
            if let Some(level) = heading_level(&name) {
                push_break(&mut output);
                output.push_str("#".repeat(level).as_str());
                output.push(' ');
                continue;
            }
            if name == "li" {
                push_break(&mut output);
                output.push_str("- ");
                continue;
            }
        }
        push_break(&mut output);
    }

    finalize_text(&output)
}

fn strip_comments(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        match after.find("-->") {
            Some(end) => rest = &after[end + 3..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
}

/// Split `<tag ...>` inner text into `(closing, lowercase name, self-closing)`.
fn parse_tag(inner: &str) -> (bool, String, bool) {
    let trimmed = inner.trim_start();
    let (closing, body) = match trimmed.strip_prefix('/') {
        Some(body) => (true, body),
        None => (false, trimmed),
    };
    let name_end = body
        .find(|character: char| character.is_whitespace() || character == '/')
        .unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let self_closing = body.trim_end().ends_with('/');
    (closing, name, self_closing)
}

fn heading_level(name: &str) -> Option<usize> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn push_break(output: &mut String) {
    while output.ends_with(' ') || output.ends_with('\t') {
        output.pop();
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn finalize_text(text: &str) -> String {
    let mut result = String::new();
    for line in text.split('\n') {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&collapsed);
    }
    result
}

/// Decode an HTML entity at the start of `text`; returns the character and the
/// number of bytes consumed.
fn decode_entity(text: &str) -> Option<(char, usize)> {
    let end = text.find(';')?;
    if end > 34 {
        return None;
    }
    let body = &text[1..end];
    if body.is_empty() {
        return None;
    }
    let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
    } else if let Some(decimal) = body.strip_prefix('#') {
        decimal.parse::<u32>().ok().and_then(char::from_u32)
    } else {
        named_entity(body)
    }?;
    Some((decoded, end + 1))
}

fn named_entity(name: &str) -> Option<char> {
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "middot" => '\u{b7}',
        "bull" => '\u{2022}',
        "deg" => '\u{b0}',
        "plusmn" => '\u{b1}',
        "times" => '\u{d7}',
        "divide" => '\u{f7}',
        "euro" => '\u{20ac}',
        "pound" => '\u{a3}',
        "yen" => '\u{a5}',
        "cent" => '\u{a2}',
        "sect" => '\u{a7}',
        "para" => '\u{b6}',
        _ => return None,
    };
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_and_style_without_leaking_content() {
        let html = "<html><head><style>p{color:red}</style><script>alert('x')</script></head>\
                    <body><h1>Title</h1><p>Hello &amp; welcome</p></body></html>";
        let text = html_to_text(html);
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
        assert!(!text.contains('<'));
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
    }

    #[test]
    fn markdown_fallback_adds_heading_and_list_markers() {
        let html = "<h2>Section</h2><ul><li>One</li><li>Two</li></ul><p>Plain</p>";
        let markdown = html_to_markdown(html);
        assert!(markdown.contains("## Section"), "{markdown}");
        assert!(markdown.contains("- One"), "{markdown}");
        assert!(markdown.contains("- Two"), "{markdown}");
    }

    #[test]
    fn decodes_numeric_and_named_entities() {
        let text = html_to_text("<p>&#65;&#x42;&lt;tag&gt; a&mdash;b &copy;</p>");
        assert_eq!(text, "AB<tag> a\u{2014}b \u{a9}");
    }

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

    fn make_target(domain: Option<&str>, addresses: Vec<IpAddr>, port: u16) -> Target {
        Target {
            url: Url::parse("http://example.com/").expect("test url"),
            domain: domain.map(str::to_string),
            port,
            addresses,
        }
    }

    #[test]
    fn dns_pins_cover_every_vetted_address() {
        let target = make_target(
            Some("example.com"),
            vec![
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)),
            ],
            8080,
        );
        let (domain, pins) = dns_pins(&target).expect("domain target should be pinned");
        assert_eq!(domain, "example.com");
        assert_eq!(pins.len(), 2);
        assert!(pins.contains(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            8080
        )));
        assert!(pins.contains(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35)),
            8080
        )));
        // The builder path consumes those pins.
        assert!(build_client(Duration::from_secs(1), &target).is_ok());
    }

    #[test]
    fn literal_targets_need_no_dns_pin() {
        let target = make_target(None, vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))], 80);
        assert!(dns_pins(&target).is_none());
        assert!(build_client(Duration::from_secs(1), &target).is_ok());
    }

    #[test]
    fn deprecated_site_local_ipv6_is_blocked() {
        assert!(is_blocked_ip("fec0::1".parse().expect("ip")));
        assert!(is_blocked_ip("fef0::1".parse().expect("ip")));
        assert!(!is_blocked_ip("2606:4700::1111".parse().expect("ip")));
    }

    #[tokio::test]
    async fn validate_target_blocks_localhost_and_literals_without_dns() {
        let url = Url::parse("http://localhost:8080/").expect("url");
        let error = validate_target(&url, url.as_str()).await.unwrap_err();
        assert!(error.message.contains("blocked"), "{}", error.message);

        for text in ["http://169.254.169.254/", "http://[fec0::1]/"] {
            let url = Url::parse(text).expect("url");
            let error = validate_target(&url, text).await.unwrap_err();
            assert!(error.message.contains("blocked"), "{}", error.message);
        }
    }
}
