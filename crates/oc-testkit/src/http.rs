//! HTTP + SSE helpers used by end-to-end tests.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

/// Send an optional JSON body and decode the JSON response.
///
/// # Errors
///
/// Returns an error for transport failures or a non-JSON body.
pub async fn json_request(
    client: &Client,
    method: Method,
    url: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("request {url}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("read body from {url}"))?;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .with_context(|| format!("decode JSON from {url} (status {status})"))?
    };
    Ok((status, value))
}

/// Create a session in the server's default location.
///
/// # Errors
///
/// Returns an error when the request fails or the response is malformed.
pub async fn create_session(client: &Client, base_url: &str) -> anyhow::Result<String> {
    let url = format!("{base_url}/api/session");
    let (status, body) = json_request(client, Method::POST, &url, Some(json!({}))).await?;
    if !status.is_success() {
        bail!("create session failed with {status}: {body}");
    }
    body.pointer("/data/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("create session response has no data.id: {body}"))
}

/// Admit a user prompt and return the HTTP status.
///
/// # Errors
///
/// Returns an error when the request fails.
pub async fn prompt(
    client: &Client,
    base_url: &str,
    session_id: &str,
    text: &str,
) -> anyhow::Result<StatusCode> {
    let url = format!("{base_url}/api/session/{session_id}/prompt");
    let (status, body) =
        json_request(client, Method::POST, &url, Some(json!({"text": text}))).await?;
    if !status.is_success() {
        bail!("prompt failed with {status}: {body}");
    }
    Ok(status)
}

/// Poll until the session is idle and at least one assistant message exists.
///
/// Returns the full transcript ordered oldest first.
///
/// # Errors
///
/// Returns an error after ten seconds.
pub async fn wait_for_execution(
    client: &Client,
    base_url: &str,
    session_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let active_url = format!("{base_url}/api/session/active");
    let messages_url = format!("{base_url}/api/session/{session_id}/message?order=asc");
    loop {
        let (_, active) = json_request(client, Method::GET, &active_url, None).await?;
        let running = active
            .pointer("/data")
            .and_then(Value::as_object)
            .is_some_and(|sessions| sessions.contains_key(session_id));
        let (_, messages) = json_request(client, Method::GET, &messages_url, None).await?;
        let transcript: Vec<Value> = messages
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let has_assistant = transcript
            .iter()
            .any(|message| message.get("type").and_then(Value::as_str) == Some("assistant"));
        if !running && has_assistant {
            return Ok(transcript);
        }
        if Instant::now() >= deadline {
            bail!(
                "session {session_id} did not settle within 10s (running={running}, messages={})",
                transcript.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Replay a session log (`follow=false`) and return its JSON frames.
///
/// # Errors
///
/// Returns an error when the request fails.
pub async fn events_after(
    client: &Client,
    base_url: &str,
    session_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let url = format!("{base_url}/api/session/{session_id}/log?after=0&follow=false");
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("request {url}"))?;
    if !response.status().is_success() {
        bail!("log request failed with {}", response.status());
    }
    let text = response.text().await.context("read log body")?;
    Ok(parse_sse_frames(&text))
}

/// Parse `data:` frames from an SSE payload.
#[must_use]
pub fn parse_sse_frames(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .filter(|data| !data.is_empty() && *data != "[DONE]")
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

/// Text content of the last assistant message, concatenated.
#[must_use]
pub fn assistant_text(messages: &[Value]) -> Option<String> {
    let message = messages
        .iter()
        .rev()
        .find(|message| message.get("type").and_then(Value::as_str) == Some("assistant"))?;
    let text = message
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    Some(text)
}

/// Incremental reader for a live SSE endpoint such as `/api/event`.
#[derive(Debug)]
pub struct SseReader {
    response: reqwest::Response,
    buffer: String,
}

impl SseReader {
    /// Open an SSE connection and return before the first frame is consumed.
    ///
    /// # Errors
    ///
    /// Returns an error for transport failures or a non-success status.
    pub async fn open(client: &Client, url: &str) -> anyhow::Result<Self> {
        let response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .with_context(|| format!("open SSE {url}"))?;
        if !response.status().is_success() {
            bail!("SSE {url} failed with {}", response.status());
        }
        Ok(Self {
            response,
            buffer: String::new(),
        })
    }

    /// Read the next JSON frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream ends or `timeout` elapses.
    pub async fn next(&mut self, timeout: Duration) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(event) = self.take_event() {
                return Ok(event);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| anyhow!("timed out waiting for an SSE frame"))?;
            let chunk = tokio::time::timeout(remaining, self.response.chunk())
                .await
                .map_err(|_| anyhow!("timed out waiting for an SSE frame"))?
                .context("read SSE chunk")?;
            match chunk {
                Some(bytes) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                None => bail!("SSE stream ended"),
            }
        }
    }

    /// Read frames until `predicate` matches.
    ///
    /// # Errors
    ///
    /// Returns an error when the stream ends or the overall timeout elapses.
    pub async fn until(
        &mut self,
        timeout: Duration,
        mut predicate: impl FnMut(&Value) -> bool,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| anyhow!("timed out waiting for a matching SSE frame"))?;
            let event = self.next(remaining).await?;
            if predicate(&event) {
                return Ok(event);
            }
        }
    }

    fn take_event(&mut self) -> Option<Value> {
        loop {
            let index = self.buffer.find("\n\n")?;
            let frame: String = self.buffer.drain(..index + 2).collect();
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str(&data) {
                return Some(value);
            }
        }
    }
}
