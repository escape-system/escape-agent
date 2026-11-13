//! Hermetic end-to-end tests for `code_map`'s ripwire delegation.
//!
//! `#![forbid(unsafe_code)]` is workspace policy, and edition 2024 makes
//! `std::env::set_var` unsafe, so this file never mutates its own environment.
//! Instead the parent test re-executes **this same test binary** with the
//! environment it needs (`RIPWIRE_PATH`, `PATH`, `HOME`,
//! `OPENCODE_RIPWIRE_TIMEOUT_MS`) and reads a JSON marker from the child's
//! stdout. `code_map_ripwire_child_harness` is the child side; it no-ops in a
//! normal run.
//!
//! Every ripwire "binary" is a shell script written into a temp dir, so the
//! suite passes on a machine without ripwire and cannot accidentally exercise
//! a real install.
//!
//! The argv assertions lock the measured ripwire 0.6.8 contract:
//!
//! - `--for` ignores `--max-tokens` and `--top-k` (both only warn on stderr)
//!   and reads `--token-budget`.
//! - the default map reads `--max-tokens` and `--top-k`.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;

use oc_tools::code_map::{
    RIPWIRE_INSTALL_CMD, SemanticPrecisionOracle, SemanticRequest, collect_post_edit_radar,
    with_semantic_oracle,
};
use oc_tools::{CodeMapTool, SymbolEditTool, Tool, ToolContext};

const CHILD_HARNESS: &str = "code_map_ripwire_child_harness";
const RESULT_MARKER: &str = "CM_RIPWIRE_RESULT:";

fn ctx(dir: &Path) -> ToolContext {
    ToolContext {
        session_id: "ses_test".to_string(),
        directory: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        cancel: None,
    }
}

/// Write an executable shell script and return its path.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

fn seed_source(dir: &Path) {
    fs::write(
        dir.join("server.rs"),
        r#"
pub struct HttpServer { port: u16 }

impl HttpServer {
    pub async fn start(&self) { println!("listening"); }
}

fn internal_worker() { println!("work"); }
"#,
    )
    .unwrap();
}

struct ChildOutcome {
    ok: bool,
    output: String,
    metadata: Value,
    error: Option<String>,
}

/// Run the child harness with a controlled environment and parse its result.
///
/// `query` travels as an env value (not argv) so a task containing spaces
/// stays a single argument, exactly like the tool accepts it.
fn run_child(
    env: &[(&str, Option<&Path>)],
    query: Option<&str>,
    timeout_ms: Option<&str>,
) -> ChildOutcome {
    let exe = std::env::current_exe().unwrap();
    let mut command = Command::new(exe);
    // libtest: one exact test, no capture so the marker reaches stdout.
    command
        .arg(CHILD_HARNESS)
        .arg("--exact")
        .arg("--nocapture")
        .arg("--test-threads=1");
    match query {
        Some(query) => {
            command.env("CM_QUERY", query);
        }
        None => {
            command.env_remove("CM_QUERY");
        }
    }
    for (key, value) in env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    if let Some(timeout_ms) = timeout_ms {
        command.env("OPENCODE_RIPWIRE_TIMEOUT_MS", timeout_ms);
    } else {
        command.env_remove("OPENCODE_RIPWIRE_TIMEOUT_MS");
    }

    let output = command.output().expect("child test binary runs");
    assert!(
        output.status.success(),
        "child harness failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = extract_marker(&stdout)
        .unwrap_or_else(|| panic!("child harness emitted no marker line; stdout was:\n{stdout}"));
    let value: Value = serde_json::from_str(line).expect("marker line is JSON");
    ChildOutcome {
        ok: value["ok"].as_bool().expect("ok is a bool"),
        output: value["output"].as_str().unwrap_or_default().to_string(),
        metadata: value["metadata"].clone(),
        error: value["error"].as_str().map(ToString::to_string),
    }
}

/// libtest writes `test <name> ... ` before captured output on the same line,
/// so the marker is located by substring rather than line prefix.
fn extract_marker(stdout: &str) -> Option<&str> {
    for line in stdout.lines() {
        if let Some(index) = line.find(RESULT_MARKER) {
            let rest = &line[index + RESULT_MARKER.len()..];
            // The payload is the JSON object up to the last closing brace; a
            // trailing `ok` for the test result may follow on its own line.
            let end = rest.rfind('}')?;
            return Some(&rest[..=end]);
        }
    }
    None
}

// One linear narrative: each block reuses the same fixture scripts and the
// same re-exec harness, so splitting per scenario would duplicate that setup
// and make the measured-contract ordering harder to follow.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn code_map_ripwire_delegation_contract() {
    let fixtures = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_source(work.path());

    let echo = script(
        fixtures.path(),
        "ripwire-argv-echo",
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done\n",
    );
    let noisy = script(
        fixtures.path(),
        "ripwire-noisy",
        "#!/bin/sh\nprintf 'warning: deprecated flag\\n' >&2\nprintf '<ctx>ok</ctx>\\n'\n",
    );
    let failing = script(
        fixtures.path(),
        "ripwire-failing",
        "#!/bin/sh\nprintf 'ripwire: refused the flag\\n' >&2\nexit 2\n",
    );
    let slow = script(
        fixtures.path(),
        "ripwire-slow",
        "#!/bin/sh\n/bin/sleep 10\n",
    );

    // The child only needs the fixture dir; it seeds nothing itself.
    let base_env: &[(&str, Option<&Path>)] = &[("CM_WORKDIR", Some(work.path()))];

    // ---- empty query: default map mode --------------------------------
    let empty = run_child(
        &[base_env[0], ("RIPWIRE_PATH", Some(echo.as_path()))],
        None,
        None,
    );
    assert!(empty.ok, "empty-query run failed: {:?}", empty.error);
    assert_eq!(empty.metadata["engine"], "ripwire");
    assert!(
        empty.metadata.get("ripwire_install_command").is_none(),
        "an installed binary must not advertise its install command: {}",
        empty.metadata
    );
    assert!(
        !empty.output.contains("--for"),
        "empty query must not pass --for: {}",
        empty.output
    );
    assert!(
        empty.output.contains("--max-tokens=1200"),
        "default map mode must carry the token budget: {}",
        empty.output
    );
    assert!(
        empty.output.contains("--top-k=7"),
        "default map mode must carry --top-k from the tool limit: {}",
        empty.output
    );
    assert!(
        !empty.output.contains(RIPWIRE_INSTALL_CMD),
        "no install tip when the binary ran: {}",
        empty.output
    );

    // ---- query present: task lens mode --------------------------------
    let query = "server start";
    let with_query = run_child(
        &[base_env[0], ("RIPWIRE_PATH", Some(echo.as_path()))],
        Some(query),
        None,
    );
    assert!(with_query.ok, "query run failed: {:?}", with_query.error);
    let output = with_query.output;
    assert!(
        output.contains(&format!("--for={query}")),
        "query must reach --for: {output}"
    );
    assert!(
        output.contains("--token-budget=1200"),
        "task lens must use the budget flag it reads: {output}"
    );
    // Measured on ripwire 0.6.8: both of these are ignored under --for and warn.
    assert!(
        !output.contains("--max-tokens"),
        "--max-tokens is not read by --for; passing it only warns: {output}"
    );
    assert!(
        !output.contains("--top-k"),
        "--top-k is not read by --for; passing it only warns: {output}"
    );
    assert!(
        with_query.metadata.get("ripwire_install_command").is_none(),
        "no install metadata when ripwire ran: {}",
        with_query.metadata
    );

    // ---- stderr is surfaced, never swallowed --------------------------
    let noisy_run = run_child(
        &[base_env[0], ("RIPWIRE_PATH", Some(noisy.as_path()))],
        None,
        None,
    );
    assert!(noisy_run.ok, "noisy run failed: {:?}", noisy_run.error);
    assert_eq!(noisy_run.metadata["engine"], "ripwire");
    assert_eq!(
        noisy_run.metadata["ripwire_stderr"], "warning: deprecated flag",
        "stderr must reach metadata even on success: {}",
        noisy_run.metadata
    );

    // ---- a failing binary falls back without an install tip ------------
    let failing_run = run_child(
        &[base_env[0], ("RIPWIRE_PATH", Some(failing.as_path()))],
        None,
        None,
    );
    assert!(
        failing_run.ok,
        "failing run failed: {:?}",
        failing_run.error
    );
    assert_eq!(failing_run.metadata["engine"], "native");
    assert!(
        failing_run
            .metadata
            .get("ripwire_install_command")
            .is_none(),
        "an installed-but-failing binary must not advertise installation: {}",
        failing_run.metadata
    );
    assert!(
        failing_run.metadata["ripwire_error"]
            .as_str()
            .is_some_and(|error| error.contains("status 2")),
        "the exit status must be reported: {}",
        failing_run.metadata
    );
    assert!(
        failing_run.metadata["ripwire_stderr"]
            .as_str()
            .is_some_and(|stderr| stderr.contains("refused the flag")),
        "the failing run's stderr must be reported: {}",
        failing_run.metadata
    );

    // ---- timeout: an error, not a hang, not a masked success -----------
    let started = Instant::now();
    let timed_out = run_child(
        &[base_env[0], ("RIPWIRE_PATH", Some(slow.as_path()))],
        Some("server"),
        Some("100"),
    );
    let elapsed = started.elapsed();
    assert!(!timed_out.ok, "a killed child must not report success");
    assert!(
        elapsed < Duration::from_secs(5),
        "the timeout must cut the run short, took {elapsed:?}"
    );
    let error = timed_out.error.unwrap();
    assert!(
        error.contains("did not finish within 100 ms"),
        "timeout error must name the budget: {error}"
    );
    assert_eq!(timed_out.metadata["engine"], "ripwire");

    // ---- no executable binary: native fallback WITH install tip --------
    // RIPWIRE_PATH names a non-executable file, HOME names an empty dir and
    // PATH is reduced to it, so no real install can leak into discovery.
    let empty_dir = TempDir::new().unwrap();
    let not_executable = empty_dir.path().join("ripwire");
    fs::write(&not_executable, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&not_executable).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&not_executable, perms).unwrap();

    let native = run_child(
        &[
            base_env[0],
            ("RIPWIRE_PATH", Some(not_executable.as_path())),
            ("HOME", Some(empty_dir.path())),
            ("PATH", Some(empty_dir.path())),
        ],
        Some("server"),
        None,
    );
    assert!(native.ok, "native run failed: {:?}", native.error);
    assert_eq!(native.metadata["engine"], "native");
    assert_eq!(
        native.metadata["ripwire_install_command"],
        json!(RIPWIRE_INSTALL_CMD)
    );
    assert!(
        native.output.contains(RIPWIRE_INSTALL_CMD),
        "native fallback must print the tip when no binary exists: {}",
        native.output
    );
}

fn build_fake_mcp_script(
    spawn_log: &Path,
    legend_log: &Path,
    for_args_log: &Path,
    work_dir: &Path,
) -> String {
    format!(
        r#"#!/usr/bin/env python3
import json, os, sys

with open({spawn_log:?}, "a") as f:
    f.write("spawn\n")

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    rid = msg.get("id")
    if method == "initialize":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}}}), flush=True)
    elif method == "resources/read":
        uri = msg["params"]["uri"]
        with open({legend_log:?}, "a") as f:
            f.write(uri + "\n")
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"contents":[{{"uri":uri,"text":"dict"}}]}}}}), flush=True)
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"]["arguments"]
        if name == "analyze":
            txt = '<r><f p="server.rs"><s t="cls" n="HttpServer"/><s t="method" n="start"/><s t="fn" n="internal_worker"/></f><f p="extra.rs"><s t="fn" n="caller_fn"/></f><!-- files=2 symbols=4 --></r>'
        elif name == "for":
            with open({for_args_log:?}, "a") as f:
                f.write(json.dumps(sorted(args.keys())) + "\n")
            txt = '<ctx task="server start"><sigs><d l="3" n="HttpServer" p="server.rs">pub struct HttpServer</d></sigs></ctx>'
        elif name == "explore":
            if "partition" in args:
                n = int(args["partition"])
                parts = "".join(f'<bundle role="partition" i="{{i}}"><f p="extra.rs"><d l="1" n="caller_fn" p="extra.rs:1"/></f></bundle>' for i in range(n))
                txt = f'<ctx-partitions task="pack"><bundle role="core"><d l="2" n="HttpServer" p="server.rs:2"/></bundle>{{parts}}</ctx-partitions>'
            else:
                txt = '<ctx task="pack"><sigs><d l="2" n="HttpServer" p="server.rs">pub struct HttpServer</d></sigs><bodies><b l="2" p="server.rs" n="HttpServer">pub struct HttpServer {{ port: u16 }}</b></bodies></ctx>'
        elif name == "lego":
            txt = '<lego type="Handler"><contract>trait Handler</contract><impl p="server.rs:2" n="HttpServer"/></lego>'
        elif name == "slice":
            txt = '<slice sym="internal_worker" var="port"><hit l="8" kind="use"/></slice>'
        elif name == "cochange":
            txt = json.dumps({{"file":"server.rs","partners":[{{"file":"extra.rs","count":4}}]}})
        elif name == "doc_drift":
            txt = '<doc_drift><miss file="README.md" line="4" target="missing_fn"/></doc_drift>'
        elif name == "flags":
            txt = '<flags><gate file="server.rs" line="1" name="EXPERIMENTAL_TLS"/></flags>'
        elif name == "quality_delta":
            txt = json.dumps({{"regressions":[{{"kind":"complexity","sym":"internal_worker","p":"server.rs:8"}}]}})
        elif name == "situational_awareness":
            txt = json.dumps({{"tests_to_run":["tests/api_test.rs"],"forgotten":[{{"file":"extra.rs"}}],"blast_radius":["extra.rs"]}})
        elif name == "edit_check":
            txt = json.dumps({{"status":"ok","symbol":args.get("symbol",""),"callers":1,"preview":bool("new_body" in args)}})
        elif name in ("replace_symbol_body", "insert_before_symbol", "insert_after_symbol"):
            if "post_check" in args and args["post_check"] != "false":
                print(json.dumps({{"jsonrpc":"2.0","id":rid,"error":{{"code":-32602,"message":"invalid value for field: post_check"}}}}), flush=True)
                continue
            sym = args.get("symbol", "")
            target = os.path.join({work_dir_str:?}, args.get("file", "server.rs"))
            with open(target, "r") as tf:
                src_text = tf.read()
            if name == "replace_symbol_body":
                updated = src_text + "\n// replaced:" + sym + "\n"
            elif name == "insert_before_symbol":
                updated = "// before:" + sym + "\n" + src_text
            else:
                updated = src_text + "\n// after:" + sym + "\n"
            with open(target, "w") as tf:
                tf.write(updated)
            txt = json.dumps({{"applied":True,"symbol":sym,"file":args.get("file","server.rs"),"lines":"1..2","edit_check":{{"status":"ok"}},"tests_to_run":["tests/api_test.rs"]}})
        elif name == "find_symbol":
            sym = args.get("symbol", "")
            if sym == "HttpServer":
                txt = json.dumps({{"symbol":{{"name":"HttpServer","kind":"cls","file":"server.rs","line":2,"handle":"sym#1234"}},"calledBy":[],"calls":[]}})
            else:
                txt = json.dumps({{"symbol":{{"name":"internal_worker","kind":"fn","file":"server.rs","line":8,"handle":"sym#5678"}},"calledBy":[{{"name":"caller_fn"}}],"calls":[]}})
        elif name == "fetch_body":
            txt = json.dumps({{"name":"internal_worker","kind":"fn","file":"server.rs","line":8,"start_line":1,"end_line":3,"body":"fn internal_worker() {{\n    // helper comment\n    println!(\"work\");\n}}"}})
        elif name == "impact":
            txt = '<impact><s t="fn" n="caller_fn" p="extra.rs:1" d="1"/></impact>'
        elif name == "uses":
            txt = '<uses><about count="0"/></uses>'
        elif name == "grep":
            txt = json.dumps({{"hits":[{{"file":"server.rs","line":5,"in":"start","text":"println!(\"listening\");"}}]}})
        elif name == "connect":
            txt = '<connect><t n="caller_fn"/><t n="internal_worker"/></connect>'
        elif name == "affected":
            txt = '<affected changed="server.rs"><test p="tests/api_test.rs"/></affected>'
        elif name == "from_trace":
            txt = '<ctx task="trace"><sigs><d l="8" n="internal_worker" p="server.rs"/></sigs></ctx>'
        elif name == "batch":
            txt = '<batch><q i="0" verb="find_symbol" ok="1">HttpServer</q><q i="1" verb="impact" ok="1">caller_fn</q></batch>'
        else:
            txt = f"ok:{{name}}"
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"content":[{{"type":"text","text":txt}}],"isError":False}}}}), flush=True)
"#,
        spawn_log = spawn_log.to_string_lossy().into_owned(),
        legend_log = legend_log.to_string_lossy().into_owned(),
        for_args_log = for_args_log.to_string_lossy().into_owned(),
        work_dir_str = work_dir.to_string_lossy().into_owned(),
    )
}

#[tokio::test]
async fn code_map_mcp_persistent_pool_and_all_operations() {
    let fixtures = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_source(work.path());
    fs::write(
        work.path().join("extra.rs"),
        "pub fn caller_fn() {\n    internal_worker();\n}\n",
    )
    .unwrap();
    let spawn_log = fixtures.path().join("mcp_spawns.log");
    let legend_log = fixtures.path().join("mcp_legends.log");
    let for_args_log = fixtures.path().join("mcp_for_args.log");

    let mcp_script = script(
        fixtures.path(),
        "ripwire-mcp-fake",
        &build_fake_mcp_script(&spawn_log, &legend_log, &for_args_log, work.path()),
    );

    let out = run_child_scenario(&[
        ("CM_WORKDIR", Some(work.path())),
        ("CM_SCENARIO", Some(Path::new("mcp_persistent_ops"))),
        ("RIPWIRE_PATH", Some(mcp_script.as_path())),
    ]);
    assert!(out.ok, "mcp persistent scenario failed: {:?}", out.error);
    assert_eq!(out.metadata["spawns"], Value::Null);
    let spawns = fs::read_to_string(&spawn_log).unwrap();
    assert_eq!(
        spawns.lines().count(),
        1,
        "MCP daemon must spawn once per root"
    );
    let legends = fs::read_to_string(&legend_log).unwrap();
    assert_eq!(
        legends.lines().collect::<Vec<_>>(),
        vec!["ripwire://legend-dict/full"]
    );
    let for_keys = fs::read_to_string(&for_args_log).unwrap();
    assert!(
        !for_keys.contains("\"limit\""),
        "MCP `for` must omit `limit` to avoid candidate-page truncation: {for_keys}"
    );
}

#[tokio::test]
async fn code_map_native_fallback_covers_all_twelve_operations() {
    let empty_dir = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    fs::create_dir_all(work.path().join("src")).unwrap();
    fs::create_dir_all(work.path().join("tests")).unwrap();
    seed_source(&work.path().join("src"));
    fs::write(
        work.path().join("src/caller.rs"),
        "pub fn api_entry() {\n    // comment\n    internal_worker();\n}\n",
    )
    .unwrap();
    fs::write(
        work.path().join("tests/api_test.rs"),
        "#[test]\nfn test_api_entry() {\n    api_entry();\n}\n",
    )
    .unwrap();

    let not_exec = empty_dir.path().join("ripwire");
    fs::write(&not_exec, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&not_exec).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&not_exec, perms).unwrap();

    let out = run_child_scenario(&[
        ("CM_WORKDIR", Some(work.path())),
        ("CM_SCENARIO", Some(Path::new("native_all_ops"))),
        ("RIPWIRE_PATH", Some(not_exec.as_path())),
        ("HOME", Some(empty_dir.path())),
        ("PATH", Some(empty_dir.path())),
    ]);
    assert!(out.ok, "native all-ops scenario failed: {:?}", out.error);
}

#[tokio::test]
async fn code_map_scip_overlay_launches_daemon_with_scip_flag() {
    let fixtures = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    seed_source(work.path());
    fs::write(work.path().join("index.scip"), b"fake-scip-bytes").unwrap();
    let argv_log = fixtures.path().join("scip_argv.log");

    let mcp_script = script(
        fixtures.path(),
        "ripwire-scip-fake",
        &format!(
            r#"#!/usr/bin/env python3
import json, sys

with open({argv_log:?}, "a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    rid = msg.get("id")
    if method == "initialize":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}}}), flush=True)
    elif method == "resources/read":
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"contents":[{{"uri":msg["params"]["uri"],"text":"dict"}}]}}}}), flush=True)
    elif method == "tools/call":
        txt = '<impact><s t="fn" n="caller_fn" p="extra.rs:1" d="1"/></impact>'
        print(json.dumps({{"jsonrpc":"2.0","id":rid,"result":{{"content":[{{"type":"text","text":txt}}],"isError":False}}}}), flush=True)
"#,
            argv_log = argv_log.to_string_lossy().into_owned(),
        ),
    );

    let out = run_child_scenario(&[
        ("CM_WORKDIR", Some(work.path())),
        ("CM_SCENARIO", Some(Path::new("scip_overlay"))),
        ("RIPWIRE_PATH", Some(mcp_script.as_path())),
    ]);
    assert!(out.ok, "scip overlay scenario failed: {:?}", out.error);
    let logged = fs::read_to_string(&argv_log).unwrap();
    assert!(
        logged.contains("--scip="),
        "expected --scip=<path> in daemon argv: {logged}"
    );
}

struct MockSemanticOracle;

#[async_trait]
impl SemanticPrecisionOracle for MockSemanticOracle {
    async fn query(&self, req: SemanticRequest) -> Result<Value, String> {
        match req {
            SemanticRequest::IncomingCalls { .. } => Ok(json!([
                {
                    "from": {
                        "name": "lsp_resolved_caller",
                        "uri": "file:///workspace/src/caller.rs",
                        "range": { "start": { "line": 4, "character": 0 } }
                    }
                }
            ])),
            _ => Ok(json!([])),
        }
    }
}

fn run_child_scenario(env: &[(&str, Option<&Path>)]) -> ChildOutcome {
    run_child(env, None, None)
}

/// Child side of the re-exec harness. Runs one `code_map` call (or a multi-step
/// scenario when `CM_SCENARIO` is set) under whatever environment the parent
/// supplied and prints a single JSON marker line. No-ops in a normal test run.
#[tokio::test]
async fn code_map_ripwire_child_harness() {
    let Some(workdir) = std::env::var_os("CM_WORKDIR") else {
        return;
    };
    let directory = PathBuf::from(workdir);
    if let Ok(scenario) = std::env::var("CM_SCENARIO") {
        let outcome = run_harness_scenario(&directory, &scenario).await;
        let payload = match outcome {
            Ok(meta) => {
                json!({ "ok": true, "output": "ok", "metadata": meta, "error": Value::Null })
            }
            Err(err) => json!({ "ok": false, "output": "", "metadata": Value::Null, "error": err }),
        };
        println!("{RESULT_MARKER}{payload}");
        return;
    }

    let query = std::env::var("CM_QUERY").ok();
    let mut input = json!({"operation": "map", "limit": 7, "max_tokens": 1200});
    if let Some(query) = &query {
        input["query"] = json!(query);
    }

    let outcome = CodeMapTool.execute(&ctx(&directory), input).await;
    let payload = match outcome {
        Ok(result) => json!({
            "ok": true,
            "output": result.output.unwrap_or_default(),
            "metadata": result.metadata.unwrap_or(Value::Null),
            "error": Value::Null,
        }),
        Err(error) => json!({
            "ok": false,
            "output": "",
            "metadata": error.metadata.unwrap_or(Value::Null),
            "error": error.message,
        }),
    };
    println!("{RESULT_MARKER}{payload}");
}

async fn run_harness_scenario(directory: &Path, scenario: &str) -> Result<Value, String> {
    let context = ctx(directory);
    match scenario {
        "mcp_persistent_ops" => run_mcp_persistent_scenario(&context, directory).await,
        "scip_overlay" => run_scip_overlay_scenario(&context).await,
        "native_all_ops" => run_native_all_ops_scenario(&context, directory).await,
        other => Err(format!("unknown scenario: {other}")),
    }
}

async fn run_code_map(
    context: &ToolContext,
    payload: Value,
) -> Result<oc_tools::ToolResult, String> {
    CodeMapTool
        .execute(context, payload)
        .await
        .map_err(|e| e.message)
}

async fn run_mcp_persistent_scenario(
    context: &ToolContext,
    directory: &Path,
) -> Result<Value, String> {
    let empty_map = run_code_map(
        context,
        json!({ "operation": "map", "max_tokens": 1200, "limit": 50 }),
    )
    .await?;
    let map_out = empty_map.output.unwrap_or_default();
    if !map_out.contains("HttpServer") || !map_out.contains("caller_fn") {
        return Err(format!("empty map lost symbols: {map_out}"));
    }
    let focused_map = run_code_map(
        context,
        json!({ "operation": "map", "query": "server start" }),
    )
    .await?;
    if !focused_map.output.unwrap_or_default().contains("<ctx") {
        return Err("focused map missing <ctx".to_string());
    }
    let packed = run_code_map(
        context,
        json!({ "operation": "pack_task", "query": "server" }),
    )
    .await?;
    if !packed.output.unwrap_or_default().contains("<bodies>") {
        return Err("pack_task missing bodies".to_string());
    }
    let sym = run_code_map(
        context,
        json!({ "operation": "symbol", "query": "HttpServer" }),
    )
    .await?;
    if !sym.output.unwrap_or_default().contains("[struct]") {
        return Err("symbol missing normalized [struct]".to_string());
    }
    let expanded = run_code_map(
        context,
        json!({ "operation": "expand", "query": "internal_worker", "compress": true }),
    )
    .await?;
    let expanded_txt = expanded.output.unwrap_or_default();
    if expanded_txt.contains("// helper comment") || !expanded_txt.contains("println!") {
        return Err(format!("expand compression failed: {expanded_txt}"));
    }
    let refs = run_code_map(
        context,
        json!({ "operation": "references", "query": "listening" }),
    )
    .await?;
    if !refs.output.unwrap_or_default().contains("server.rs") {
        return Err("references grep fallback failed".to_string());
    }
    for simple in [
        json!({ "operation": "impact", "query": "internal_worker" }),
        json!({ "operation": "connect", "query": "caller_fn -> internal_worker" }),
        json!({ "operation": "affected", "query": "server.rs" }),
        json!({ "operation": "trace", "query": "at server.rs:8" }),
    ] {
        let _ = run_code_map(context, simple).await?;
    }
    let batch = run_code_map(
        context,
        json!({
            "operation": "batch",
            "queries": ["find_symbol:HttpServer", "impact:internal_worker"]
        }),
    )
    .await?;
    if !batch.output.unwrap_or_default().contains("<batch>") {
        return Err("batch missing <batch>".to_string());
    }

    verify_mcp_deep_ops(context).await?;
    verify_mcp_symbol_edits(context, directory).await?;
    Ok(json!({ "verified": true }))
}

async fn verify_mcp_deep_ops(context: &ToolContext) -> Result<(), String> {
    let partitioned = run_code_map(
        context,
        json!({
            "operation": "pack_task",
            "query": "server",
            "partitions": 2
        }),
    )
    .await?;
    if !partitioned
        .output
        .unwrap_or_default()
        .contains("<ctx-partitions")
    {
        return Err("pack_task partitions missing <ctx-partitions".to_string());
    }

    for op_payload in [
        json!({ "operation": "lego", "query": "Handler" }),
        json!({ "operation": "slice", "query": "internal_worker", "var": "port", "flow": "both", "depth": 4 }),
        json!({ "operation": "cochange", "query": "server.rs" }),
        json!({ "operation": "doc_drift" }),
        json!({ "operation": "flags" }),
        json!({ "operation": "quality", "query": "delta" }),
    ] {
        let outcome = run_code_map(context, op_payload.clone()).await?;
        if outcome.output.unwrap_or_default().trim().is_empty() {
            return Err(format!("empty output for MCP deep op {op_payload}"));
        }
    }
    Ok(())
}

async fn verify_mcp_symbol_edits(context: &ToolContext, directory: &Path) -> Result<(), String> {
    let preview = SymbolEditTool
        .execute(
            context,
            json!({ "operation": "preview", "symbol": "HttpServer", "path": "server.rs", "content": "pub struct HttpServer;" }),
        )
        .await
        .map_err(|e| e.message)?;
    if preview
        .metadata
        .as_ref()
        .and_then(|m| m["operation"].as_str())
        != Some("preview")
    {
        return Err("symbol_edit preview metadata mismatch".to_string());
    }
    for edit_payload in [
        json!({ "operation": "replace", "symbol": "internal_worker", "path": "server.rs", "content": "fn internal_worker() {}" }),
        json!({ "operation": "insert_before", "symbol": "internal_worker", "path": "server.rs", "content": "// header", "post_check": false }),
        json!({ "operation": "insert_after", "symbol": "internal_worker", "path": "server.rs", "content": "// footer" }),
    ] {
        let outcome = SymbolEditTool
            .execute(context, edit_payload.clone())
            .await
            .map_err(|e| e.message)?;
        let meta = outcome.metadata.unwrap_or(Value::Null);
        if meta["files"].as_array().is_none_or(Vec::is_empty) {
            return Err(format!(
                "symbol_edit missing diff receipt for {edit_payload}"
            ));
        }
    }

    let radar = collect_post_edit_radar(directory, &["server.rs".to_string()], true).await;
    if !radar.is_actionable()
        || radar.suggested_tests.is_empty()
        || radar.forgotten_partners.is_empty()
        || radar.regressions.is_empty()
    {
        return Err(format!("MCP post-edit radar missed signals: {radar:?}"));
    }
    Ok(())
}

async fn run_scip_overlay_scenario(context: &ToolContext) -> Result<Value, String> {
    let outcome = run_code_map(
        context,
        json!({
            "operation": "impact",
            "query": "internal_worker",
            "precision": "scip"
        }),
    )
    .await?;
    let meta = outcome.metadata.unwrap_or(Value::Null);
    if meta["scip_overlay"] != json!(true) {
        return Err(format!("expected scip_overlay=true in metadata: {meta}"));
    }
    Ok(json!({ "verified": true }))
}

async fn run_native_all_ops_scenario(
    context: &ToolContext,
    directory: &Path,
) -> Result<Value, String> {
    for payload in [
        json!({ "operation": "pack_task", "query": "api_entry" }),
        json!({ "operation": "pack_task", "query": "api_entry", "partitions": 2 }),
        json!({ "operation": "map" }),
        json!({ "operation": "map", "query": "api_entry", "rank_by": "coupling" }),
        json!({ "operation": "communities" }),
        json!({ "operation": "outline", "path": "src" }),
        json!({ "operation": "symbol", "query": "api_entry" }),
        json!({ "operation": "expand", "query": "api_entry", "compress": true }),
        json!({ "operation": "impact", "query": "internal_worker" }),
        json!({ "operation": "references", "query": "listening" }),
        json!({ "operation": "connect", "query": "api_entry -> internal_worker" }),
        json!({ "operation": "affected", "query": "internal_worker" }),
        json!({ "operation": "trace", "query": "src/caller.rs:3" }),
        json!({ "operation": "batch", "queries": ["symbol:api_entry", "impact:internal_worker"] }),
        json!({ "operation": "lego", "query": "HttpServer" }),
        json!({ "operation": "slice", "query": "api_entry" }),
        json!({ "operation": "cochange", "query": "src/caller.rs" }),
        json!({ "operation": "doc_drift" }),
        json!({ "operation": "flags" }),
        json!({ "operation": "quality", "query": "delta" }),
        json!({ "operation": "quality", "query": "hotspots" }),
        json!({ "operation": "quality", "query": "dead_code" }),
    ] {
        let outcome = run_code_map(context, payload.clone()).await?;
        if outcome.output.unwrap_or_default().trim().is_empty() {
            return Err(format!("empty output for {payload}"));
        }
    }

    let lsp_output = with_semantic_oracle(Some(Arc::new(MockSemanticOracle)), async {
        run_code_map(
            context,
            json!({
                "operation": "impact",
                "query": "internal_worker",
                "precision": "lsp"
            }),
        )
        .await
    })
    .await?;
    let lsp_text = lsp_output.output.unwrap_or_default();
    if !lsp_text.contains("<lsp_precision") || !lsp_text.contains("lsp_resolved_caller") {
        return Err(format!("missing LSP precision section: {lsp_text}"));
    }

    let native_radar =
        collect_post_edit_radar(directory, &["src/caller.rs".to_string()], false).await;
    if !native_radar
        .suggested_tests
        .iter()
        .any(|t| t.contains("api_test.rs"))
    {
        return Err(format!("native radar missed api_test.rs: {native_radar:?}"));
    }
    Ok(json!({ "verified": true }))
}
