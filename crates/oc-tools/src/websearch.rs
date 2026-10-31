//! `websearch` tool: zero-config web search (`DuckDuckGo` HTML + optional `SearXNG` JSON).
//!
//! By default, queries `https://html.duckduckgo.com/html/` and extracts result
//! titles, canonical URLs, and snippets into compact Markdown. When the
//! `OC_SEARXNG_URL` environment variable is set, queries `{OC_SEARXNG_URL}/search?format=json`
//! first and falls back to `DuckDuckGo` HTML if needed.

use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE};
use serde_json::{Value, json};
use url::Url;

use crate::util;
use crate::webfetch::html_to_text;
use crate::{Tool, ToolContext, ToolError, ToolResult};

/// Default number of search results returned.
pub const DEFAULT_NUM_RESULTS: usize = 8;
/// Maximum number of search results returned.
pub const MAX_NUM_RESULTS: usize = 20;
/// Default request timeout in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 20;
/// Upper bound for the request timeout in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 60;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// A single extracted search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// `websearch` tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebsearchTool;

#[async_trait]
impl Tool for WebsearchTool {
    fn name(&self) -> &str {
        "websearch"
    }

    fn description(&self) -> &str {
        "Search the web for a query and return top results (title, URL, and snippet) in Markdown. Works out of the box without API keys via DuckDuckGo HTML search, or uses a custom SearXNG instance when `OC_SEARXNG_URL` is set. Follow up with `webfetch` on a returned URL when you need the full page content."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query to look up on the web"
                },
                "numResults": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_NUM_RESULTS,
                    "description": "Maximum number of search results to return (default 8, maximum 20)"
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_TIMEOUT_SECONDS,
                    "description": "Optional timeout in seconds (default 20, maximum 60)"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, _ctx: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let query = util::required_string(&input, "query")?;
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(ToolError::new(
                "Invalid arguments: `query` must not be empty",
            ));
        }
        let num_results = match util::optional_u64(&input, "numResults")? {
            Some(0) => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: `numResults` must be between 1 and {MAX_NUM_RESULTS}"
                )));
            }
            Some(value) if value > MAX_NUM_RESULTS as u64 => {
                return Err(ToolError::new(format!(
                    "Invalid arguments: `numResults` must be between 1 and {MAX_NUM_RESULTS}"
                )));
            }
            Some(value) => usize::try_from(value).unwrap_or(DEFAULT_NUM_RESULTS),
            None => DEFAULT_NUM_RESULTS,
        };
        let timeout_secs =
            util::optional_u64(&input, "timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        if timeout_secs == 0 || timeout_secs > MAX_TIMEOUT_SECONDS {
            return Err(ToolError::new(format!(
                "Invalid arguments: `timeout` must be between 1 and {MAX_TIMEOUT_SECONDS} seconds"
            )));
        }
        let timeout = Duration::from_secs(timeout_secs);

        let client = Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| ToolError::new(format!("Unable to create HTTP client: {error}")))?;

        let (hits, provider) = if let Ok(searxng) = std::env::var("OC_SEARXNG_URL")
            && !searxng.trim().is_empty()
        {
            match search_searxng(&client, searxng.trim(), trimmed, num_results).await {
                Ok(hits) if !hits.is_empty() => (hits, "searxng"),
                _ => (
                    search_ddg(&client, trimmed, num_results).await?,
                    "duckduckgo",
                ),
            }
        } else {
            (
                search_ddg(&client, trimmed, num_results).await?,
                "duckduckgo",
            )
        };

        let output = format_hits(trimmed, &hits);
        let metadata = json!({
            "query": trimmed,
            "provider": provider,
            "count": hits.len(),
        });
        Ok(ToolResult::new(Some(output), None, Some(metadata)))
    }
}

async fn search_searxng(
    client: &Client,
    base_url: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let mut url = Url::parse(base_url)
        .map_err(|error| ToolError::new(format!("Invalid OC_SEARXNG_URL `{base_url}`: {error}")))?;
    if !url.path().ends_with("/search") {
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{trimmed}/search"));
    }
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json");

    let response = client
        .get(url)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| ToolError::new(format!("SearXNG request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ToolError::new(format!(
            "SearXNG returned HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|error| ToolError::new(format!("Failed to read SearXNG response: {error}")))?;
    parse_searxng_json(&body, limit)
}

async fn search_ddg(
    client: &Client,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let mut url =
        Url::parse("https://html.duckduckgo.com/html/").expect("static DuckDuckGo URL is valid");
    url.query_pairs_mut().append_pair("q", query);

    let response = client
        .get(url)
        .header(
            ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|error| ToolError::new(format!("Web search request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(ToolError::new(format!(
            "Web search returned HTTP {}",
            response.status()
        )));
    }
    let html = response
        .text()
        .await
        .map_err(|error| ToolError::new(format!("Failed to read web search response: {error}")))?;
    Ok(parse_ddg_html(&html, limit))
}

/// Parse `SearXNG` JSON output into up to `limit` [`SearchHit`]s.
pub fn parse_searxng_json(body: &str, limit: usize) -> Result<Vec<SearchHit>, ToolError> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|error| ToolError::new(format!("Invalid SearXNG JSON: {error}")))?;
    let Some(items) = parsed.get("results").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut hits = Vec::new();
    for item in items {
        if hits.len() >= limit {
            break;
        }
        let Some(url) = item.get("url").and_then(Value::as_str) else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or(url)
            .to_owned();
        let snippet = item
            .get("content")
            .and_then(Value::as_str)
            .map(|text| html_to_text(text).replace('\n', " "))
            .unwrap_or_default();
        hits.push(SearchHit {
            title,
            url: url.trim().to_owned(),
            snippet: snippet.trim().to_owned(),
        });
    }
    Ok(hits)
}

/// Parse `DuckDuckGo` HTML (`html.duckduckgo.com/html/`) results into up to `limit` [`SearchHit`]s.
pub fn parse_ddg_html(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut rest = html;
    while hits.len() < limit {
        let Some(anchor_pos) = find_result_anchor(rest) else {
            break;
        };
        rest = &rest[anchor_pos..];
        let Some(tag_end) = rest.find('>') else {
            break;
        };
        let open_tag = &rest[..tag_end];
        let after_open = &rest[tag_end + 1..];
        let Some(close_a) = after_open.find("</a>") else {
            rest = after_open;
            continue;
        };
        let raw_title = &after_open[..close_a];
        let raw_href = extract_attr(open_tag, "href").unwrap_or_default();
        rest = &after_open[close_a + 4..];

        let next_anchor = find_result_anchor(rest).unwrap_or(rest.len());
        let window = &rest[..next_anchor];
        let snippet = extract_snippet(window);

        let url = unwrap_ddg_url(&raw_href);
        if url.is_empty() || url.starts_with("https://duckduckgo.com/y.js") {
            continue;
        }
        let title = html_to_text(raw_title).replace('\n', " ");
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title: title.to_owned(),
            url,
            snippet,
        });
    }
    hits
}

fn find_result_anchor(haystack: &str) -> Option<usize> {
    haystack
        .find("class=\"result__a\"")
        .or_else(|| haystack.find("class='result__a'"))
        .and_then(|class_pos| haystack[..class_pos].rfind("<a"))
}

fn extract_snippet(window: &str) -> String {
    let Some(class_pos) = window
        .find("class=\"result__snippet\"")
        .or_else(|| window.find("class='result__snippet'"))
    else {
        return String::new();
    };
    let after_class = &window[class_pos..];
    let Some(tag_end) = after_class.find('>') else {
        return String::new();
    };
    let body = &after_class[tag_end + 1..];
    let close = body
        .find("</a>")
        .or_else(|| body.find("</div>"))
        .or_else(|| body.find("</span>"))
        .unwrap_or(body.len());
    html_to_text(&body[..close])
        .replace('\n', " ")
        .trim()
        .to_owned()
}

fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let double = format!("{attr}=\"");
    if let Some(start) = tag.find(&double) {
        let rest = &tag[start + double.len()..];
        let end = rest.find('"')?;
        return Some(rest[..end].replace("&amp;", "&"));
    }
    let single = format!("{attr}='");
    if let Some(start) = tag.find(&single) {
        let rest = &tag[start + single.len()..];
        let end = rest.find('\'')?;
        return Some(rest[..end].replace("&amp;", "&"));
    }
    None
}

fn unwrap_ddg_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let candidate = if trimmed.starts_with("//") {
        format!("https:{trimmed}")
    } else if trimmed.starts_with('/') {
        format!("https://duckduckgo.com{trimmed}")
    } else {
        trimmed.to_owned()
    };
    if let Ok(parsed) = Url::parse(&candidate) {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" && (value.starts_with("http://") || value.starts_with("https://")) {
                return value.into_owned();
            }
        }
    }
    if candidate.starts_with("http://") || candidate.starts_with("https://") {
        candidate
    } else {
        String::new()
    }
}

fn format_hits(query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!("No web search results found for `{query}`.");
    }
    let mut out = format!("Web search results for `{query}`:\n\n");
    for (idx, hit) in hits.iter().enumerate() {
        let _ = writeln!(out, "{}. **[{}]({})**", idx + 1, hit.title, hit.url);
        let _ = writeln!(out, "   URL: {}", hit.url);
        if !hit.snippet.is_empty() {
            let _ = writeln!(out, "   {}", hit.snippet);
        }
        out.push('\n');
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duckduckgo_html_and_unwraps_uddg_links() {
        let html = r#"
        <div class="results">
          <div class="result results_links results_links_deep web-result">
            <h2 class="result__title">
              <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=abc">Rust Programming Language</a>
            </h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">A language empowering everyone to build <b>reliable</b> and efficient software.</a>
          </div>
          <div class="result results_links results_links_deep web-result">
            <h2 class="result__title">
              <a rel="nofollow" class="result__a" href="https://tokio.rs">Tokio &amp; Async Rust</a>
            </h2>
            <div class="result__snippet">An event-driven, non-blocking I/O platform.</div>
          </div>
        </div>
        "#;
        let hits = parse_ddg_html(html, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            hits[0].snippet,
            "A language empowering everyone to build reliable and efficient software."
        );
        assert_eq!(hits[1].title, "Tokio & Async Rust");
        assert_eq!(hits[1].url, "https://tokio.rs");
        assert_eq!(
            hits[1].snippet,
            "An event-driven, non-blocking I/O platform."
        );
    }

    #[test]
    fn parses_searxng_json_results() {
        let body = r#"{
            "query": "rust",
            "results": [
                {
                    "title": "Rust Docs",
                    "url": "https://doc.rust-lang.org",
                    "content": "Official <em>Rust</em> documentation."
                }
            ]
        }"#;
        let hits = parse_searxng_json(body, 5).expect("valid searxng json");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Rust Docs");
        assert_eq!(hits[0].url, "https://doc.rust-lang.org");
        assert_eq!(hits[0].snippet, "Official Rust documentation.");
    }
}
