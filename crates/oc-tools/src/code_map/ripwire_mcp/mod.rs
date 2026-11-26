//! Lazy, bounded persistent `ripwire --mcp` daemon pool.
//!
//! Spawning `ripwire <root>` per invocation reloads the workspace graph from
//! disk (180–560 ms) and repeats 1–2 KB of XML legend definitions per response.
//! Keeping up to `MAX_DAEMONS` warm `ripwire --mcp` stdio children after first
//! use and reading `ripwire://legend-dict/full` once per session eliminates both
//! costs while keeping boot idle RSS at zero and automatically reflecting new
//! on-disk edits on subsequent queries.

mod daemon;
mod pool;
mod wire;

#[cfg(test)]
pub(super) use self::daemon::call_mcp_tool;
pub(crate) use self::daemon::call_mcp_tool_with_scip;

use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct McpToolOutput {
    pub(crate) text: String,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum McpCallError {
    /// Binary exited or emitted non-JSON-RPC on initialize; fall back to CLI.
    CliOnly { stderr: Vec<u8> },
    /// Hard wall-clock deadline expired; must not fall back silently.
    Timeout { timeout: Duration },
    /// Caller cancelled execution via `ToolContext.cancel`.
    Cancelled,
    /// Persistent MCP daemon returned an explicit JSON-RPC or tool-level error.
    Tool { message: String, stderr: Vec<u8> },
    /// Process spawn, broken pipe after retry, or protocol framing error.
    Io { message: String, stderr: Vec<u8> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn fake_mcp_daemon_reads_legend_once_and_reuses_process() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let counter_file = dir.path().join("spawns.log");
        let methods_file = dir.path().join("methods.log");
        let script = write_script(
            dir.path(),
            "fake-ripwire-mcp",
            &format!(
                r#"#!/usr/bin/env python3
import json, sys

with open({counter:?}, "a") as f:
    f.write("spawn\n")

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    with open({methods:?}, "a") as f:
        f.write(str(method) + "\n")
    rid = msg.get("id")
    if method == "initialize":
        resp = {{"jsonrpc":"2.0","id":rid,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}}}
        print(json.dumps(resp), flush=True)
    elif method == "resources/read":
        resp = {{"jsonrpc":"2.0","id":rid,"result":{{"contents":[{{"uri":"ripwire://legend-dict/full","text":"dict"}}]}} }}
        print(json.dumps(resp), flush=True)
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"]["arguments"]
        resp = {{"jsonrpc":"2.0","id":rid,"result":{{"content":[{{"type":"text","text":f"ok:{{name}}:{{args.get('symbol','')}}"}}],"isError":False}}}}
        print(json.dumps(resp), flush=True)
"#,
                counter = counter_file.to_string_lossy().into_owned(),
                methods = methods_file.to_string_lossy().into_owned(),
            ),
        );

        let cancel = CancellationToken::new();
        let timeout = Duration::from_secs(5);
        let first = call_mcp_tool(
            &script,
            &root,
            &cancel,
            timeout,
            "find_symbol",
            &json!({ "symbol": "Alpha" }),
        )
        .await
        .unwrap();
        assert_eq!(first.text, "ok:find_symbol:Alpha");

        let second = call_mcp_tool(
            &script,
            &root,
            &cancel,
            timeout,
            "impact",
            &json!({ "symbol": "Beta" }),
        )
        .await
        .unwrap();
        assert_eq!(second.text, "ok:impact:Beta");

        let spawns = fs::read_to_string(&counter_file).unwrap();
        assert_eq!(spawns.lines().count(), 1, "daemon should be spawned once");
        let methods = fs::read_to_string(&methods_file).unwrap();
        assert_eq!(
            methods.lines().collect::<Vec<_>>(),
            vec![
                "initialize",
                "notifications/initialized",
                "resources/read",
                "tools/call",
                "tools/call",
            ]
        );
    }

    #[tokio::test]
    async fn non_json_binary_detected_as_cli_only() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let script = write_script(
            dir.path(),
            "cli-only",
            "#!/usr/bin/env sh\necho 'not json rpc'\n",
        );

        let err = call_mcp_tool(
            &script,
            &root,
            &CancellationToken::new(),
            Duration::from_secs(5),
            "analyze",
            &json!({}),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, McpCallError::CliOnly { .. }),
            "unexpected error variant: {err:?}"
        );
    }

    #[tokio::test]
    async fn slow_mcp_binary_returns_timeout_and_drops_daemon() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let script = write_script(dir.path(), "slow-mcp", "#!/usr/bin/env sh\nsleep 2\n");

        let err = call_mcp_tool(
            &script,
            &root,
            &CancellationToken::new(),
            Duration::from_millis(80),
            "analyze",
            &json!({}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpCallError::Timeout { .. }));
    }
}
