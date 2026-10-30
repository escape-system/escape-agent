//! JSON-RPC 2.0 message types and framing helpers.
//!
//! MCP stdio uses newline-delimited JSON with a 16 MiB frame cap; streamable
//! HTTP responses may be JSON or server-sent events. Both decoders are
//! byte-oriented and never panic on malformed input.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{McpError, McpResult, RpcError};

/// Maximum bytes in one stdio frame (and one SSE data line).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// JSON-RPC protocol version literal.
pub const JSONRPC_VERSION: &str = "2.0";

/// Request identifier: a number or a string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{value:?}"),
        }
    }
}

/// An outgoing or incoming JSON-RPC request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Builds a request with the correct protocol literal.
    #[must_use]
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC notification (no id, no response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Builds a notification with the correct protocol literal.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC response carrying exactly one of `result` or `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// Successful response.
    #[must_use]
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response.
    #[must_use]
    pub fn failure(id: RequestId, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Any JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

impl Message {
    /// Parses one JSON text into a typed message. Unknown or malformed shapes
    /// produce [`McpError::Protocol`].
    pub fn parse(text: &str) -> McpResult<Self> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| McpError::Protocol(error.to_string()))?;
        Self::from_value(value)
    }

    /// Classifies an already parsed JSON value.
    pub fn from_value(value: Value) -> McpResult<Self> {
        let Value::Object(object) = &value else {
            return Err(McpError::Protocol(
                "message is not a JSON object".to_owned(),
            ));
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return Err(McpError::Protocol(
                "message is missing jsonrpc \"2.0\"".to_owned(),
            ));
        }
        let has_method = object.contains_key("method");
        let has_id = object.contains_key("id");
        match (has_method, has_id) {
            (true, true) => serde_json::from_value(value)
                .map(Self::Request)
                .map_err(|error| McpError::Protocol(error.to_string())),
            (true, false) => serde_json::from_value(value)
                .map(Self::Notification)
                .map_err(|error| McpError::Protocol(error.to_string())),
            (false, true) => {
                let response: Response = serde_json::from_value(value)
                    .map_err(|error| McpError::Protocol(error.to_string()))?;
                if response.result.is_none() && response.error.is_none() {
                    return Err(McpError::Protocol(
                        "response has neither result nor error".to_owned(),
                    ));
                }
                Ok(Self::Response(response))
            }
            (false, false) => Err(McpError::Protocol(
                "message is neither request, notification nor response".to_owned(),
            )),
        }
    }

    /// Serializes the message as one line of JSON (without the trailing newline).
    pub fn to_line(&self) -> McpResult<Vec<u8>> {
        let value = match self {
            Self::Request(request) => serde_json::to_value(request)?,
            Self::Notification(notification) => serde_json::to_value(notification)?,
            Self::Response(response) => serde_json::to_value(response)?,
        };
        serde_json::to_vec(&value).map_err(McpError::from)
    }

    /// The message id when it carries one.
    #[must_use]
    pub fn id(&self) -> Option<&RequestId> {
        match self {
            Self::Request(request) => Some(&request.id),
            Self::Response(response) => Some(&response.id),
            Self::Notification(_) => None,
        }
    }
}

/// Incremental newline-delimited JSON decoder with a hard frame cap.
#[derive(Debug, Default)]
pub struct NewlineDecoder {
    buffer: Vec<u8>,
    discarding: bool,
}

impl NewlineDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes and returns complete frames plus at most one capped-frame
    /// error. An oversized frame is discarded through the next newline so the
    /// stream stays usable.
    pub fn feed(&mut self, chunk: &[u8]) -> (Vec<Vec<u8>>, Option<McpError>) {
        let mut frames = Vec::new();
        let mut error = None;
        for &byte in chunk {
            if byte == b'\n' {
                if self.discarding {
                    self.discarding = false;
                    self.buffer.clear();
                    continue;
                }
                let mut frame = std::mem::take(&mut self.buffer);
                if frame.last() == Some(&b'\r') {
                    frame.pop();
                }
                if !frame.is_empty() {
                    frames.push(frame);
                }
                continue;
            }
            if self.discarding {
                continue;
            }
            self.buffer.push(byte);
            if self.buffer.len() > MAX_FRAME_BYTES {
                self.buffer.clear();
                self.discarding = true;
                if error.is_none() {
                    error = Some(McpError::FrameTooLarge);
                }
            }
        }
        (frames, error)
    }
}

/// One parsed server-sent event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: String,
}

/// Incremental server-sent-events decoder (CRLF and LF line endings).
#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    data: String,
    id: Option<String>,
    event: Option<String>,
    seen_field: bool,
}

impl SseDecoder {
    /// Creates an empty decoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes and returns every event completed by this chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let mut line = std::mem::take(&mut self.buffer);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if let Some(event) = self.process_line(&line) {
                    events.push(event);
                }
            } else {
                self.buffer.push(byte);
                if self.buffer.len() > MAX_FRAME_BYTES {
                    self.buffer.clear();
                    self.data.clear();
                    self.id = None;
                    self.event = None;
                    self.seen_field = false;
                }
            }
        }
        events
    }

    fn process_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        if line.is_empty() {
            lf_dispatch(
                &mut self.data,
                &mut self.id,
                &mut self.event,
                &mut self.seen_field,
            )
        } else if line[0] == b':' {
            None
        } else {
            let text = String::from_utf8_lossy(line);
            let (field, value) = match text.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (text.as_ref(), ""),
            };
            match field {
                "data" => {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                    self.seen_field = true;
                }
                "id" => {
                    self.id = Some(value.to_owned());
                    self.seen_field = true;
                }
                "event" => {
                    self.event = Some(value.to_owned());
                    self.seen_field = true;
                }
                _ => {}
            }
            None
        }
    }
}

fn lf_dispatch(
    data: &mut String,
    id: &mut Option<String>,
    event: &mut Option<String>,
    seen_field: &mut bool,
) -> Option<SseEvent> {
    if !*seen_field && data.is_empty() {
        return None;
    }
    let parsed = SseEvent {
        id: id.take(),
        event: event.take(),
        data: std::mem::take(data),
    };
    *seen_field = false;
    Some(parsed)
}

/// Parses a JSON-RPC payload carried by one SSE event.
pub fn sse_message(event: &SseEvent) -> McpResult<Message> {
    Message::parse(&event.data)
}

/// JSON-RPC error codes used by the client.
pub mod codes {
    /// Parse error.
    pub const PARSE_ERROR: i64 = -32700;
    /// Invalid request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// Unknown method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid params.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal error.
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_message_shapes() {
        let request =
            Message::parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).expect("request");
        assert!(matches!(request, Message::Request(_)));
        let notification =
            Message::parse(r#"{"jsonrpc":"2.0","method":"x"}"#).expect("notification");
        assert!(matches!(notification, Message::Notification(_)));
        let response =
            Message::parse(r#"{"jsonrpc":"2.0","id":"a","result":{}}"#).expect("response");
        assert!(matches!(response, Message::Response(_)));
        let error =
            Message::parse(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#)
                .expect("error response");
        assert!(matches!(error, Message::Response(_)));
    }

    #[test]
    fn rejects_malformed_without_panicking() {
        for text in [
            "",
            "null",
            "{}",
            "[]",
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":5}"#,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"",
        ] {
            assert!(Message::parse(text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn decodes_multiple_newline_frames_and_crlf() {
        let mut decoder = NewlineDecoder::new();
        let (frames, error) = decoder.feed(b"{\"a\":1}\r\n{\"b\"");
        assert_eq!(frames, vec![b"{\"a\":1}".to_vec()]);
        assert!(error.is_none());
        let (frames, _) = decoder.feed(b":2}\n");
        assert_eq!(frames, vec![b"{\"b\":2}".to_vec()]);
    }

    #[test]
    fn caps_frame_and_recovers_on_newline() {
        let mut decoder = NewlineDecoder::new();
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let (frames, error) = decoder.feed(&oversized);
        assert!(frames.is_empty());
        assert!(matches!(error, Some(McpError::FrameTooLarge)));
        let (frames, error) = decoder.feed(b"discarded\n{\"ok\":true}\r\n");
        assert!(error.is_none());
        assert_eq!(frames, vec![b"{\"ok\":true}".to_vec()]);
    }

    #[test]
    fn decodes_sse_events_with_ids_and_comments() {
        let mut decoder = SseDecoder::new();
        let events =
            decoder.feed(b": keepalive\r\nevent: message\r\nid: 4\r\ndata: {\"a\":1}\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("4"));
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, r#"{"a":1}"#);
    }

    #[test]
    fn decodes_multiline_sse_data() {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        events.extend(decoder.feed(b"data: line1\n"));
        events.extend(decoder.feed(b"data: line2\n\n"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }
}
