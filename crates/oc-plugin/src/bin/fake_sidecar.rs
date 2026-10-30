//! Test double for the JS plugin sidecar.
//!
//! Speaks the same newline-framed JSON-RPC 2.0 protocol as the real Bun/Node
//! host so `oc-plugin` can be tested without either runtime installed. Selected
//! by `FAKE_PLUGIN_SIDECAR_MODE`:
//!
//! | Mode | Behaviour |
//! |---|---|
//! | unset / `ok` | handshake, register one plugin per target, serve hooks/transforms/rpc |
//! | `unsupported` | registers `aisdk.sdk`, which the host must reject |
//! | `crash` | exits immediately |
//! | `crash_after_ready` | answers `load`, then exits |
//! | `hang` | never answers anything |
//! | `hang_after_ready` | answers `load`, then never answers again |
//! | `malformed_once` | writes one garbage frame, then behaves normally |
//! | `malformed` | writes three garbage frames, then goes silent |
//! | `oversize` | writes a frame larger than 8 MiB |
//! | `callback` | calls back into the host after `load` |
//!
//! Failure injection: a hook event with `"deny": true` is rejected; a transform
//! document with `"fail": true` is rejected; a `tool.execute.before` event with
//! `"tool": "deny"` is rejected with plugin metadata.

use std::io::{BufRead, Write};
use std::process::exit;

use serde_json::{Value, json};

#[allow(clippy::too_many_lines)]
fn main() {
    if std::env::var("FAKE_PLUGIN_SIDECAR_MODE").as_deref() == Ok("crash") {
        exit(3);
    }
    let mode = std::env::var("FAKE_PLUGIN_SIDECAR_MODE").unwrap_or_default();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    if mode == "malformed" {
        for _ in 0..3 {
            let _ = writeln!(writer, "not json");
        }
        let _ = writer.flush();
    } else if mode == "malformed_once" {
        let _ = writeln!(writer, "not json");
        let _ = writer.flush();
    } else if mode == "oversize" {
        let chunk = "x".repeat(1024 * 1024);
        for _ in 0..9 {
            let _ = write!(writer, "{chunk}");
        }
        let _ = writeln!(writer);
        let _ = writer.flush();
    }

    let mut callback_sent = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let id = frame.get("id").cloned();
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = frame.get("params").cloned().unwrap_or(Value::Null);

        if method.is_empty() {
            // Response to our callback request.
            if id.as_ref().and_then(Value::as_u64) == Some(9001) && frame.get("result").is_some() {
                notify(
                    &mut writer,
                    "log",
                    json!({"level": "info", "message": "callback.ok"}),
                );
            }
            continue;
        }

        if mode == "hang" || mode == "malformed" {
            continue;
        }
        if mode == "hang_after_ready" && method != "load" {
            continue;
        }

        match method.as_str() {
            "load" => {
                let targets: Vec<String> = params
                    .get("targets")
                    .and_then(Value::as_array)
                    .map(|targets| {
                        targets
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                notify(
                    &mut writer,
                    "log",
                    json!({"level": "info", "message": "loading plugin targets"}),
                );
                let mut hooks = vec![
                    "tool.execute.before".to_string(),
                    "tool.execute.after".to_string(),
                    "session.prompt".to_string(),
                ];
                if mode == "unsupported" {
                    hooks.push("aisdk.sdk".to_string());
                }
                for target in &targets {
                    notify(
                        &mut writer,
                        "register",
                        json!({
                            "id": target,
                            "features": {"server": true, "rpc": true},
                            "hooks": hooks.clone(),
                            "transforms": ["agent"],
                            "source": {"type": "package", "target": target, "version": "0.0.0"}
                        }),
                    );
                }
                respond(&mut writer, id.as_ref(), json!({"loaded": targets}));
                if mode == "callback" && !callback_sent {
                    callback_sent = true;
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "id": 9001,
                        "method": "callback",
                        "params": {
                            "requestId": "r1",
                            "method": "session.read",
                            "args": {"sessionID": "ses_fake"}
                        }
                    });
                    let _ = writeln!(writer, "{frame}");
                    let _ = writer.flush();
                }
                if mode == "crash_after_ready" {
                    let _ = writer.flush();
                    exit(4);
                }
            }
            "hook" => {
                let hook = params
                    .get("hook")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut event = params.get("event").cloned().unwrap_or(Value::Null);
                let deny = event.get("deny").and_then(Value::as_bool) == Some(true)
                    || (hook == "tool.execute.before"
                        && event.get("tool").and_then(Value::as_str) == Some("deny"));
                if deny {
                    respond_error(
                        &mut writer,
                        id.as_ref(),
                        -32000,
                        "blocked by fake plugin",
                        Some(json!({"plugin": "fake.pkg", "metadata": {"policy": "deny"}})),
                    );
                    continue;
                }
                if let Some(object) = event.as_object_mut() {
                    object.insert("sidecar".to_string(), json!("fake"));
                }
                respond(&mut writer, id.as_ref(), event);
            }
            "transform" => {
                let mut doc = params.get("doc").cloned().unwrap_or(Value::Null);
                if doc.get("fail").and_then(Value::as_bool) == Some(true) {
                    respond_error(
                        &mut writer,
                        id.as_ref(),
                        -32000,
                        "transform rejected",
                        Some(json!({"plugin": "fake.pkg"})),
                    );
                    continue;
                }
                if let Some(object) = doc.as_object_mut() {
                    object.insert("sidecar".to_string(), json!("fake"));
                }
                respond(&mut writer, id.as_ref(), doc);
            }
            "rpc" => {
                respond(
                    &mut writer,
                    id.as_ref(),
                    json!({
                        "namespace": params.get("namespace").cloned().unwrap_or(Value::Null),
                        "method": params.get("method").cloned().unwrap_or(Value::Null),
                        "input": params.get("input").cloned().unwrap_or(Value::Null),
                        "plugin": "fake.pkg"
                    }),
                );
            }
            "reload" => {
                respond(
                    &mut writer,
                    id.as_ref(),
                    json!({"reloaded": params.get("ids").cloned().unwrap_or(Value::Null)}),
                );
            }
            "dispose" => respond(&mut writer, id.as_ref(), Value::Null),
            "plugin.shutdown" => {
                respond(&mut writer, id.as_ref(), Value::Null);
                let _ = writer.flush();
                exit(0);
            }
            other => respond_error(
                &mut writer,
                id.as_ref(),
                -32601,
                &format!("unknown method: {other}"),
                None,
            ),
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn respond(writer: &mut impl Write, id: Option<&Value>, result: Value) {
    let Some(id) = id else {
        return;
    };
    let frame = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let _ = writeln!(writer, "{frame}");
    let _ = writer.flush();
}

fn respond_error(
    writer: &mut impl Write,
    id: Option<&Value>,
    code: i64,
    message: &str,
    data: Option<Value>,
) {
    let Some(id) = id else {
        return;
    };
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    let frame = json!({"jsonrpc": "2.0", "id": id, "error": error});
    let _ = writeln!(writer, "{frame}");
    let _ = writer.flush();
}

#[allow(clippy::needless_pass_by_value)]
fn notify(writer: &mut impl Write, method: &str, params: Value) {
    let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
    let _ = writeln!(writer, "{frame}");
    let _ = writer.flush();
}
