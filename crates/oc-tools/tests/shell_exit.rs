//! Regression tests for the exit status a pipeline reports.
//!
//! The shell tool used to report the *last* segment's status, so a failing
//! pipeline such as `cargo clippy … | tail` returned `exit: 0` and looked like
//! a success to the model and to any metadata-only auditor. The tool now runs
//! the command with `-o pipefail` where the shell supports it, so a failed
//! earlier segment makes the reported exit non-zero.

use std::fs;
use std::path::Path;

use serde_json::json;
use tempfile::TempDir;

use oc_tools::{ShellTool, Tool, ToolContext};

fn ctx(dir: &Path) -> ToolContext {
    ToolContext {
        session_id: "ses_test".to_string(),
        directory: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        cancel: None,
    }
}

#[tokio::test]
async fn failing_pipeline_reports_non_zero_exit() {
    let dir = TempDir::new().unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "exit 3 | cat", "timeout": 30_000}),
        )
        .await
        .unwrap();
    let metadata = result.metadata.unwrap();
    let exit = metadata["exit"].as_i64().expect("exit is reported");
    assert_ne!(
        exit, 0,
        "a failed pipeline segment must not be reported as success: {metadata}"
    );
    assert_eq!(exit, 3, "pipefail reports the failing segment's status");
    assert_eq!(metadata["status"], "completed");
    let output = result.output.unwrap();
    assert!(
        output.contains("Exited with code 3"),
        "the model-facing text must not claim success either: {output}"
    );
}

#[tokio::test]
async fn succeeding_pipeline_reports_zero_exit() {
    let dir = TempDir::new().unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "true | true", "timeout": 30_000}),
        )
        .await
        .unwrap();
    let metadata = result.metadata.unwrap();
    assert_eq!(metadata["exit"].as_i64(), Some(0));
    assert_eq!(metadata["status"], "completed");
    assert!(
        !result.output.unwrap().contains("Exited with code"),
        "a successful pipeline must not print an exit notice"
    );
}

/// The concrete failure mode from the audit: a non-zero command piped into a
/// filter. `tail` exits 0, so without pipefail the reported status is 0.
#[tokio::test]
async fn failing_command_piped_into_tail_reports_non_zero_exit() {
    let dir = TempDir::new().unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "sh -c 'exit 101' | tail -n 1", "timeout": 30_000}),
        )
        .await
        .unwrap();
    let metadata = result.metadata.unwrap();
    assert_eq!(
        metadata["exit"].as_i64(),
        Some(101),
        "clippy-style `… | tail` must surface the failing command's status: {metadata}"
    );
}

/// Captured output, working directory and the other metadata keys survive.
#[tokio::test]
async fn pipeline_keeps_output_and_metadata_shape() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("data.txt"), "hello\n").unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "cat data.txt | tr a-z A-Z", "timeout": 30_000}),
        )
        .await
        .unwrap();
    let metadata = result.metadata.unwrap();
    assert_eq!(result.output.unwrap().trim(), "HELLO");
    assert_eq!(metadata["exit"].as_i64(), Some(0));
    assert_eq!(metadata["truncated"].as_bool(), Some(false));
    assert!(metadata["shellID"].as_str().is_some());
    assert!(metadata["outputPath"].as_str().is_some());
    assert_eq!(metadata["status"], "completed");
}

/// `pipefail` must not leak into the shell a command can observe in ways that
/// change semantics: a plain `sh` without support still runs the command.
#[tokio::test]
async fn shell_without_pipefail_still_runs_commands() {
    let dir = TempDir::new().unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "printf '%s' \"$OPENCODE_SESSION_ID\"", "timeout": 30_000}),
        )
        .await
        .unwrap();
    assert_eq!(result.output.unwrap().trim(), "ses_test");
    assert_eq!(result.metadata.unwrap()["exit"].as_i64(), Some(0));
}
