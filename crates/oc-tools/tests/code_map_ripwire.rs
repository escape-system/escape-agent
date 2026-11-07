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
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

use oc_tools::code_map::RIPWIRE_INSTALL_CMD;
use oc_tools::{CodeMapTool, Tool, ToolContext};

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

/// Child side of the re-exec harness. Runs one `code_map` call under whatever
/// environment the parent supplied and prints a single JSON marker line.
/// No-ops (passes) in a normal test run.
#[tokio::test]
async fn code_map_ripwire_child_harness() {
    let Some(workdir) = std::env::var_os("CM_WORKDIR") else {
        return;
    };
    let directory = PathBuf::from(workdir);
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
