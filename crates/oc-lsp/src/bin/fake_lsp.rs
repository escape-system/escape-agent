//! Deterministic fake LSP server used by the integration tests.
//!
//! Controlled through environment variables:
//! - `FAKE_LSP_MODE`: `normal` (default), `crash`, `hang`, `stubborn-exit`.
//! - `FAKE_LSP_TRACE`: append every received message / notable event here.
//! - `FAKE_LSP_EXIT_MARKER`: written on a clean `exit` notification.
//!
//! It never touches the network and speaks only the subset the client needs.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use oc_lsp::rpc::{RequestId, RpcMessage, SyncFrameReader};
use serde_json::{Value, json};

fn main() -> ExitCode {
    let mode = std::env::var("FAKE_LSP_MODE").unwrap_or_else(|_| "normal".into());
    let mut server = Server {
        mode,
        trace: std::env::var("FAKE_LSP_TRACE").ok().map(PathBuf::from),
        marker: std::env::var("FAKE_LSP_EXIT_MARKER")
            .ok()
            .map(PathBuf::from),
        pending: HashMap::new(),
    };
    server.trace_line("start");

    if server.mode == "crash" {
        server.trace_line("crash");
        return ExitCode::from(3);
    }

    let stdin = std::io::stdin();
    let mut reader = SyncFrameReader::new(BufReader::new(stdin));
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        let message = match reader.read_message() {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                server.trace_line(&format!("framing-error {error}"));
                break;
            }
        };
        match message {
            RpcMessage::Request { id, method, params } => {
                server.trace_line(&format!("request {} {}", method, compact(&params)));
                if server.mode == "hang" {
                    continue;
                }
                server.handle_request(&mut out, &id, &method, &params);
            }
            RpcMessage::Notification { method, params } => {
                server.trace_line(&format!("notification {} {}", method, compact(&params)));
                if server.mode == "hang" {
                    continue;
                }
                if server.handle_notification(&mut out, &method, &params) {
                    return ExitCode::SUCCESS;
                }
            }
            RpcMessage::Response { id, result, error } => {
                let label = id
                    .as_ref()
                    .and_then(|id| server.pending.remove(id))
                    .unwrap_or("unsolicited");
                let error_json = error.as_ref().map_or(Value::Null, |error| {
                    serde_json::to_value(error).unwrap_or(Value::Null)
                });
                server.trace_line(&format!(
                    "response {label} result={} error={}",
                    compact(&result.unwrap_or(Value::Null)),
                    compact(&error_json),
                ));
            }
        }
    }

    if server.mode == "stubborn-exit" {
        std::thread::sleep(Duration::from_secs(5));
    }
    server.trace_line("eof");
    ExitCode::SUCCESS
}

struct Server {
    mode: String,
    trace: Option<PathBuf>,
    marker: Option<PathBuf>,
    pending: HashMap<RequestId, &'static str>,
}

impl Server {
    fn trace_line(&self, line: &str) {
        let Some(path) = &self.trace else {
            return;
        };
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn handle_request(
        &mut self,
        out: &mut impl Write,
        id: &RequestId,
        method: &str,
        params: &Value,
    ) {
        match method {
            "initialize" => self.initialize(out, id),
            "shutdown" => {
                send(
                    out,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }),
                );
            }
            _ => {
                if Self::handle_navigation(out, id, method, params) {
                    return;
                }
                if Self::handle_hierarchy(out, id, method, params) {
                    return;
                }
                if Self::handle_diagnostic(out, id, method, params) {
                    return;
                }
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("method not found: {method}") }
                    }),
                );
            }
        }
    }

    fn initialize(&mut self, out: &mut impl Write, id: &RequestId) {
        send(
            out,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "capabilities": {
                        "textDocumentSync": { "openClose": true, "change": 1 },
                        "hoverProvider": true,
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "implementationProvider": true,
                        "documentSymbolProvider": true,
                        "workspaceSymbolProvider": true,
                        "callHierarchyProvider": true,
                        "diagnosticProvider": {
                            "interFileDependencies": false,
                            "workspaceDiagnostics": false
                        }
                    },
                    "serverInfo": { "name": "fake-lsp", "version": "0.1.0" }
                }
            }),
        );
        self.request_from_client(out, 900, "configuration");
        self.request_from_client(out, 901, "progress");
        self.request_from_client(out, 902, "register");
        self.request_from_client(out, 903, "folders");
    }

    fn handle_navigation(
        out: &mut impl Write,
        id: &RequestId,
        method: &str,
        params: &Value,
    ) -> bool {
        match method {
            "textDocument/hover" => {
                let (line, character) = position_of(params);
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "contents": {
                                "kind": "markdown",
                                "value": format!("hover {line}:{character}")
                            },
                            "range": range(0, 1, 0, 3)
                        }
                    }),
                );
            }
            "textDocument/definition" | "textDocument/implementation" => {
                let uri = text_document_uri(params);
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "uri": uri,
                            "range": range(0, 0, 0, 4)
                        }]
                    }),
                );
            }
            "textDocument/references" => {
                let uri = text_document_uri(params);
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [
                            { "uri": uri, "range": range(0, 0, 0, 2) },
                            { "uri": uri, "range": range(1, 0, 1, 2) }
                        ]
                    }),
                );
            }
            "textDocument/documentSymbol" => {
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "name": "fake_symbol",
                            "kind": 12,
                            "range": range(0, 0, 2, 0),
                            "selectionRange": range(0, 3, 0, 8)
                        }]
                    }),
                );
            }
            "workspace/symbol" => {
                let query = params
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let symbols: Vec<Value> = (0..60)
                    .map(|index| {
                        json!({
                            "name": format!("{query}_{index}"),
                            "kind": 12,
                            "location": {
                                "uri": "file:///workspace/fake.rs",
                                "range": range(index, 0, index, 1)
                            }
                        })
                    })
                    .collect();
                send(
                    out,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": symbols }),
                );
            }
            _ => return false,
        }
        true
    }

    fn handle_hierarchy(
        out: &mut impl Write,
        id: &RequestId,
        method: &str,
        params: &Value,
    ) -> bool {
        match method {
            "textDocument/prepareCallHierarchy" => {
                let uri = text_document_uri(params);
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "name": "fake_fn",
                            "kind": 12,
                            "uri": uri,
                            "range": range(0, 0, 3, 0),
                            "selectionRange": range(0, 3, 0, 10)
                        }]
                    }),
                );
            }
            "callHierarchy/incomingCalls" | "callHierarchy/outgoingCalls" => {
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": [{
                            "from": {
                                "name": "caller",
                                "kind": 12,
                                "uri": "file:///workspace/fake.rs",
                                "range": range(0, 0, 1, 0),
                                "selectionRange": range(0, 3, 0, 9)
                            },
                            "fromRanges": [range(0, 0, 0, 3)]
                        }]
                    }),
                );
            }
            _ => return false,
        }
        true
    }

    fn handle_diagnostic(
        out: &mut impl Write,
        id: &RequestId,
        method: &str,
        params: &Value,
    ) -> bool {
        if method != "textDocument/diagnostic" {
            return false;
        }
        let previous = params.get("previousResultId").and_then(Value::as_str);
        if previous == Some("fake-1") {
            send(
                out,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "kind": "unchanged", "resultId": "fake-1" }
                }),
            );
        } else {
            send(
                out,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "kind": "full",
                        "resultId": "fake-1",
                        "items": sample_diagnostics("pull error")
                    }
                }),
            );
        }
        true
    }

    fn request_from_client(&mut self, out: &mut impl Write, id: i64, kind: &'static str) {
        self.pending.insert(RequestId::Number(id), kind);
        let (method, params) = match kind {
            "configuration" => (
                "workspace/configuration",
                json!({ "items": [{ "section": "fake" }] }),
            ),
            "progress" => (
                "window/workDoneProgress/create",
                json!({ "token": "fake-token" }),
            ),
            "register" => (
                "client/registerCapability",
                json!({ "registrations": [{ "id": "watcher", "method": "workspace/didChangeWatchedFiles", "registerOptions": { "watchers": [{ "globPattern": "**/*.fake" }] } }] }),
            ),
            _ => ("workspace/workspaceFolders", json!({})),
        };
        self.trace_line(&format!("send {method}"));
        send(
            out,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
    }

    fn handle_notification(&self, out: &mut impl Write, method: &str, params: &Value) -> bool {
        match method {
            "textDocument/didOpen" | "textDocument/didChange" => {
                let document = &params["textDocument"];
                let uri = document.get("uri").cloned().unwrap_or(Value::Null);
                let version = document.get("version").and_then(Value::as_i64).unwrap_or(0);
                send(
                    out,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": {
                            "uri": uri,
                            "version": version,
                            "diagnostics": sample_diagnostics(&format!("fake error v{version}"))
                        }
                    }),
                );
            }
            "exit" => {
                self.trace_line("exit");
                if self.mode == "stubborn-exit" {
                    return false;
                }
                if let Some(marker) = &self.marker {
                    let _ = std::fs::write(marker, std::process::id().to_string());
                }
                return true;
            }
            _ => {}
        }
        false
    }
}

fn sample_diagnostics(message: &str) -> Value {
    json!([
        {
            "range": range(0, 1, 0, 3),
            "severity": 1,
            "code": "E001",
            "source": "fake",
            "message": message
        },
        {
            "range": range(1, 0, 1, 4),
            "severity": 2,
            "code": "W001",
            "source": "fake",
            "message": "fake warning"
        }
    ])
}

fn range(start_line: usize, start_char: usize, end_line: usize, end_char: usize) -> Value {
    json!({
        "start": { "line": start_line, "character": start_char },
        "end": { "line": end_line, "character": end_char }
    })
}

fn position_of(params: &Value) -> (u64, u64) {
    let position = &params["position"];
    (
        position.get("line").and_then(Value::as_u64).unwrap_or(0),
        position
            .get("character")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

fn text_document_uri(params: &Value) -> Value {
    params
        .get("textDocument")
        .and_then(|document| document.get("uri"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn compact(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 240 {
        let mut end = 240;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    } else {
        text
    }
}

fn send(out: &mut impl Write, message: &Value) {
    if let Ok(frame) = oc_lsp::rpc::encode_message(message) {
        let _ = out.write_all(&frame);
        let _ = out.flush();
    }
}
