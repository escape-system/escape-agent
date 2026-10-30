//! JSON-RPC 2.0 over stdio with LSP `Content-Length` framing.
//!
//! The parser is deliberately defensive: malformed headers produce an error,
//! never a panic, and bodies are capped so a broken server cannot exhaust RAM.

use std::fmt;
use std::io::BufRead;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum bytes of header block accepted before the blank line.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Maximum body size accepted from a server (64 MiB).
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Framing or JSON-RPC transport failure.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// Underlying I/O failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A header line was malformed or truncated.
    #[error("malformed message header: {0}")]
    MalformedHeader(String),

    /// No `Content-Length` header was present.
    #[error("message is missing the Content-Length header")]
    MissingContentLength,

    /// `Content-Length` was not a valid byte count.
    #[error("invalid Content-Length header: {0}")]
    InvalidContentLength(String),

    /// Declared body exceeds `MAX_BODY_BYTES`.
    #[error("message body too large: {0} bytes")]
    BodyTooLarge(usize),

    /// Body was not valid JSON.
    #[error("invalid JSON message: {0}")]
    Json(#[from] serde_json::Error),

    /// A JSON value was not a JSON-RPC request, notification or response.
    #[error("invalid JSON-RPC message: {0}")]
    InvalidMessage(String),

    /// The connection closed while a request was outstanding.
    #[error("LSP connection closed")]
    Closed,

    /// A request did not complete in time.
    #[error("LSP request `{0}` timed out")]
    Timeout(String),

    /// The peer returned a JSON-RPC error object.
    #[error("JSON-RPC error {code}: {message}")]
    Server { code: i64, message: String },
}

/// JSON-RPC request id: integer or string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric id.
    Number(i64),
    /// String id.
    String(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{value}"),
        }
    }
}

impl From<i64> for RequestId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcErrorObject {
    /// Error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Additional data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A decoded JSON-RPC message.
#[derive(Debug, Clone)]
pub enum RpcMessage {
    /// Client or server request.
    Request {
        /// Request id.
        id: RequestId,
        /// Method name.
        method: String,
        /// Raw params.
        params: Value,
    },
    /// Notification (no id, no response).
    Notification {
        /// Method name.
        method: String,
        /// Raw params.
        params: Value,
    },
    /// Response to an earlier request.
    Response {
        /// Id of the request being answered.
        id: Option<RequestId>,
        /// Success result.
        result: Option<Value>,
        /// Failure.
        error: Option<RpcErrorObject>,
    },
}

/// Parse a JSON value into a [`RpcMessage`].
pub fn parse_message(value: Value) -> Result<RpcMessage, RpcError> {
    let Value::Object(object) = value else {
        return Err(RpcError::InvalidMessage("expected an object".into()));
    };
    if let Some(method) = object.get("method") {
        let Some(method) = method.as_str() else {
            return Err(RpcError::InvalidMessage("method is not a string".into()));
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        return match object.get("id").cloned() {
            Some(Value::Null) | None => Ok(RpcMessage::Notification {
                method: method.to_owned(),
                params,
            }),
            Some(id) => {
                let id = serde_json::from_value::<RequestId>(id).map_err(|error| {
                    RpcError::InvalidMessage(format!("invalid request id: {error}"))
                })?;
                Ok(RpcMessage::Request {
                    id,
                    method: method.to_owned(),
                    params,
                })
            }
        };
    }
    if let Some(id) = object.get("id") {
        let id = if id.is_null() {
            None
        } else {
            Some(
                serde_json::from_value::<RequestId>(id.clone()).map_err(|error| {
                    RpcError::InvalidMessage(format!("invalid response id: {error}"))
                })?,
            )
        };
        let result = object.get("result").cloned();
        let error = match object.get("error") {
            None | Some(Value::Null) => None,
            Some(error) => Some(
                serde_json::from_value::<RpcErrorObject>(error.clone()).map_err(|error| {
                    RpcError::InvalidMessage(format!("invalid error object: {error}"))
                })?,
            ),
        };
        if result.is_none() && error.is_none() {
            return Err(RpcError::InvalidMessage(
                "response has neither `result` nor `error`".into(),
            ));
        }
        return Ok(RpcMessage::Response { id, result, error });
    }
    Err(RpcError::InvalidMessage(
        "missing both `method` and `id`".into(),
    ))
}

/// Encode a payload with an LSP header block.
#[must_use]
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 32);
    frame.extend_from_slice(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Serialize a JSON-RPC message into a framed byte buffer.
pub fn encode_message(message: &Value) -> Result<Vec<u8>, RpcError> {
    let payload = serde_json::to_vec(message)?;
    Ok(encode_frame(&payload))
}

/// Write a framed message and flush.
pub async fn write_message<W>(writer: &mut W, message: &Value) -> Result<(), RpcError>
where
    W: AsyncWrite + Unpin,
{
    let frame = encode_message(message)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

fn header_block(mut block: Vec<u8>) -> Result<usize, RpcError> {
    // Some servers pad with CRLF; be liberal about a trailing newline.
    while block
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        block.pop();
    }
    if block.is_empty() {
        return Err(RpcError::MissingContentLength);
    }
    let text = std::str::from_utf8(&block)
        .map_err(|_| RpcError::MalformedHeader("header is not UTF-8".into()))?;
    let mut length = None;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(RpcError::MalformedHeader(format!(
                "missing colon in `{line}`"
            )));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            if length.is_some() {
                return Err(RpcError::MalformedHeader(
                    "duplicate Content-Length header".into(),
                ));
            }
            let parsed = value.parse::<usize>().map_err(|_| {
                RpcError::InvalidContentLength(format!("`{value}` is not a byte count"))
            })?;
            length = Some(parsed);
        }
        // Content-Type and unknown headers are ignored on purpose.
    }
    let length = length.ok_or(RpcError::MissingContentLength)?;
    if length > MAX_BODY_BYTES {
        return Err(RpcError::BodyTooLarge(length));
    }
    Ok(length)
}

/// Blocking frame reader over any [`BufRead`] (used by the fake test server).
#[derive(Debug)]
pub struct SyncFrameReader<R> {
    inner: R,
}

impl<R: BufRead> SyncFrameReader<R> {
    /// Wrap a reader.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read one frame, or `None` on clean EOF before a header.
    pub fn read_frame(&mut self) -> Result<Option<Vec<u8>>, RpcError> {
        let mut block = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = self.inner.read_until(b'\n', &mut line)?;
            if read == 0 {
                if block.is_empty() {
                    return Ok(None);
                }
                return Err(RpcError::MalformedHeader("truncated header".into()));
            }
            if block.len() + line.len() > MAX_HEADER_BYTES {
                return Err(RpcError::MalformedHeader("header block too large".into()));
            }
            if line.as_slice() == b"\r\n" || line.as_slice() == b"\n" {
                break;
            }
            block.extend_from_slice(&line);
        }
        let length = header_block(block)?;
        let mut body = vec![0_u8; length];
        self.inner.read_exact(&mut body)?;
        Ok(Some(body))
    }

    /// Read and decode one message.
    pub fn read_message(&mut self) -> Result<Option<RpcMessage>, RpcError> {
        match self.read_frame()? {
            None => Ok(None),
            Some(body) => parse_message(serde_json::from_slice(&body)?).map(Some),
        }
    }
}

/// Async frame reader over the child's stdout.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    /// Wrap a reader.
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Read one frame, or `None` on clean EOF before a header.
    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, RpcError> {
        let mut block = Vec::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = self.inner.read_until(b'\n', &mut line).await?;
            if read == 0 {
                if block.is_empty() {
                    return Ok(None);
                }
                return Err(RpcError::MalformedHeader("truncated header".into()));
            }
            if block.len() + line.len() > MAX_HEADER_BYTES {
                return Err(RpcError::MalformedHeader("header block too large".into()));
            }
            if line.as_slice() == b"\r\n" || line.as_slice() == b"\n" {
                break;
            }
            block.extend_from_slice(&line);
        }
        let length = header_block(block)?;
        let mut body = vec![0_u8; length];
        self.inner.read_exact(&mut body).await?;
        Ok(Some(body))
    }

    /// Read and decode one message.
    pub async fn read_message(&mut self) -> Result<Option<RpcMessage>, RpcError> {
        match self.read_frame().await? {
            None => Ok(None),
            Some(body) => parse_message(serde_json::from_slice(&body)?).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_round_trip() {
        let message = serde_json::json!({"jsonrpc": "2.0", "id": 7, "method": "x", "params": {}});
        let frame = encode_message(&message).unwrap();
        let mut reader = SyncFrameReader::new(std::io::Cursor::new(frame));
        let decoded = reader.read_message().unwrap().unwrap();
        match decoded {
            RpcMessage::Request { id, method, .. } => {
                assert_eq!(id, RequestId::Number(7));
                assert_eq!(method, "x");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn accepts_extra_headers_and_case_insensitive_names() {
        let payload = br#"{"jsonrpc":"2.0","method":"n"}"#;
        let frame = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\nX-Other: 1\r\n\r\n",
            payload.len()
        );
        let mut bytes = frame.into_bytes();
        bytes.extend_from_slice(payload);
        let mut reader = SyncFrameReader::new(std::io::Cursor::new(bytes));
        assert!(matches!(
            reader.read_message().unwrap().unwrap(),
            RpcMessage::Notification { .. }
        ));
    }

    #[test]
    fn malformed_headers_error_without_panicking() {
        let cases: &[&[u8]] = &[
            b"Content-Length: x\r\n\r\n{}",
            b"Content-Length: 1\r\nContent-Length: 1\r\n\r\n{",
            b"Nonsense\r\n\r\n{}",
            b"Content-Length: 999999999999999999999\r\n\r\n{}",
            b"\xb0\xad\r\n\r\n",
        ];
        for case in cases {
            let mut reader = SyncFrameReader::new(std::io::Cursor::new(*case));
            assert!(reader.read_frame().is_err(), "case {case:?}");
        }
    }

    #[test]
    fn missing_content_length_is_an_error() {
        let mut reader =
            SyncFrameReader::new(std::io::Cursor::new(b"Content-Type: x\r\n\r\n{}".to_vec()));
        assert!(matches!(
            reader.read_frame(),
            Err(RpcError::MissingContentLength)
        ));
    }

    #[test]
    fn truncated_header_and_body_are_errors() {
        let mut reader =
            SyncFrameReader::new(std::io::Cursor::new(b"Content-Length: 10\r\n".to_vec()));
        assert!(matches!(
            reader.read_frame(),
            Err(RpcError::MalformedHeader(_))
        ));
        let mut reader = SyncFrameReader::new(std::io::Cursor::new(
            b"Content-Length: 10\r\n\r\n{}".to_vec(),
        ));
        assert!(reader.read_frame().is_err());
    }

    #[test]
    fn parses_responses_with_errors_and_null_ids() {
        let value =
            serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-1,"message":"bad"}});
        match parse_message(value).unwrap() {
            RpcMessage::Response {
                id: None,
                error: Some(error),
                ..
            } => {
                assert_eq!(error.code, -1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_non_jsonrpc_values() {
        for value in [
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!({"id": 1}),
        ] {
            assert!(parse_message(value).is_err());
        }
    }
}
