//! Fake MCP stdio server used by `oc-mcp` integration tests.
//!
//! Speaks newline-delimited JSON-RPC on stdin/stdout and exercises initialize,
//! tools/list pagination, tools/call (including errors and every content kind),
//! resources, prompts, ping, a server-initiated `roots/list`, and shutdown via
//! stdin EOF.

use std::io::{BufRead, Write};

use serde_json::{Map, Value, json};

#[allow(
    clippy::too_many_lines,
    reason = "single-file fake server keeps every handler in one readable dispatch"
)]
fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut pending_roots: Option<Value> = None;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            let _ = write_line(
                &mut stdout,
                &json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {"code": -32700, "message": "parse error"}
                }),
            );
            continue;
        };

        // A response from the client (to our roots/list request).
        if message.get("method").is_none() {
            if message.get("id").and_then(Value::as_str) == Some("srv-roots-1")
                && let Some(tool_id) = pending_roots.take()
            {
                let roots = message
                    .get("result")
                    .and_then(|result| result.get("roots"))
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                let _ = write_line(&mut stdout, &tool_result(&tool_id, &roots.to_string()));
            }
            continue;
        }

        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        if id.is_none() {
            // notifications/initialized, notifications/cancelled, ...
            continue;
        }
        let id = id.unwrap_or(Value::Null);

        match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-11-25");
                let _ = write_line(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": version,
                            "capabilities": {
                                "tools": {"listChanged": true},
                                "resources": {"listChanged": true},
                                "prompts": {"listChanged": true}
                            },
                            "serverInfo": {
                                "name": "fake-mcp",
                                "version": "1.0.0",
                                "title": "Fake MCP Server"
                            },
                            "instructions": "Fake server instructions.\nSecond line."
                        }
                    }),
                );
            }
            "ping" => {
                let _ = write_line(
                    &mut stdout,
                    &json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                );
            }
            "tools/list" => {
                let cursor = params.get("cursor").and_then(Value::as_str);
                let result = match cursor {
                    None => json!({
                        "tools": [{
                            "name": "echo",
                            "description": "Echo arguments",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"value": {"type": "string"}},
                                "required": ["value"]
                            }
                        }],
                        "nextCursor": "page-2"
                    }),
                    Some("page-2") => json!({
                        "tools": [
                            {
                                "name": "fail",
                                "description": "Always fails",
                                "inputSchema": {"type": "object", "properties": {}}
                            },
                            {
                                "name": "media",
                                "description": "Every content kind",
                                "inputSchema": {"type": "object", "properties": {}}
                            },
                            {
                                "name": "structured",
                                "description": "Structured output",
                                "inputSchema": {"type": "object", "properties": {}},
                                "outputSchema": {"type": "object"}
                            },
                            {
                                "name": "broken",
                                "description": "Missing input schema, must be dropped"
                            }
                        ]
                    }),
                    Some(other) => {
                        let _ =
                            write_error(&mut stdout, &id, -32602, &format!("bad cursor {other}"));
                        continue;
                    }
                };
                let _ = write_line(
                    &mut stdout,
                    &json!({"jsonrpc": "2.0", "id": id, "result": result}),
                );
            }
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match name {
                    "echo" => {
                        let text = format!("echo:{arguments}");
                        let _ = write_line(&mut stdout, &tool_result(&id, &text));
                    }
                    "fail" => {
                        let _ = write_line(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "isError": true,
                                    "content": [{"type": "text", "text": "boom from fake server"}]
                                }
                            }),
                        );
                    }
                    "media" => {
                        let _ = write_line(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [
                                        {"type": "text", "text": "text part"},
                                        {"type": "image", "data": "aW1n", "mimeType": "image/png"},
                                        {"type": "audio", "data": "YXVk", "mimeType": "audio/wav"},
                                        {"type": "resource_link", "uri": "file:///tmp/link"},
                                        {
                                            "type": "resource",
                                            "resource": {"uri": "file:///inline", "text": "inline text"}
                                        }
                                    ]
                                }
                            }),
                        );
                    }
                    "structured" => {
                        let _ = write_line(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [],
                                    "structuredContent": {"ok": true, "count": 2}
                                }
                            }),
                        );
                    }
                    "roots" => {
                        // Exercise the client's roots/list handler.
                        pending_roots = Some(id);
                        let _ = write_line(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": "srv-roots-1",
                                "method": "roots/list"
                            }),
                        );
                    }
                    other => {
                        let _ =
                            write_error(&mut stdout, &id, -32602, &format!("unknown tool {other}"));
                    }
                }
            }
            "resources/list" => {
                let _ = write_line(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "resources": [{
                                "uri": "file:///fake.txt",
                                "name": "fake.txt",
                                "description": "A fake resource",
                                "mimeType": "text/plain"
                            }]
                        }
                    }),
                );
            }
            "resources/templates/list" => {
                let _ = write_line(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "resourceTemplates": [{
                                "uriTemplate": "file:///{path}",
                                "name": "file"
                            }]
                        }
                    }),
                );
            }
            "resources/read" => {
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let _ = write_line(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "contents": [{
                                "uri": uri,
                                "mimeType": "text/plain",
                                "text": "fake contents"
                            }]
                        }
                    }),
                );
            }
            "prompts/list" => {
                let _ = write_line(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "prompts": [{
                                "name": "greet",
                                "description": "Greets someone",
                                "arguments": [{"name": "who", "required": true}]
                            }]
                        }
                    }),
                );
            }
            "prompts/get" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let who = params
                    .get("arguments")
                    .and_then(|arguments| arguments.get("who"))
                    .and_then(Value::as_str)
                    .unwrap_or("world");
                if name == "greet" {
                    let _ = write_line(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "description": "greeting",
                                "messages": [{
                                    "role": "user",
                                    "content": {"type": "text", "text": format!("hello {who}")}
                                }]
                            }
                        }),
                    );
                } else {
                    let _ = write_error(&mut stdout, &id, -32602, "unknown prompt");
                }
            }
            other => {
                let _ = write_error(
                    &mut stdout,
                    &id,
                    -32601,
                    &format!("method not found: {other}"),
                );
            }
        }
    }
}

fn tool_result(id: &Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": text}]}
    })
}

fn write_error(
    stdout: &mut std::io::Stdout,
    id: &Value,
    code: i64,
    message: &str,
) -> std::io::Result<()> {
    let mut error = Map::new();
    error.insert("code".to_owned(), Value::from(code));
    error.insert("message".to_owned(), Value::from(message));
    write_line(
        stdout,
        &json!({"jsonrpc": "2.0", "id": id, "error": Value::Object(error)}),
    )
}

fn write_line(stdout: &mut std::io::Stdout, value: &Value) -> std::io::Result<()> {
    let encoded = serde_json::to_string(value).map_err(std::io::Error::other)?;
    stdout.write_all(encoded.as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
