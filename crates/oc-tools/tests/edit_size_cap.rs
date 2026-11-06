//! Regression test: `edit` must refuse a file larger than `MAX_FILE_BYTES`
//! instead of reading it into memory unbounded.

use std::fs;
use std::path::Path;

use serde_json::json;
use tempfile::TempDir;

use oc_tools::read::MAX_FILE_BYTES;
use oc_tools::{EditTool, PatchTool, Tool, ToolContext, ToolError, ToolResult};

fn ctx(dir: &Path) -> ToolContext {
    ToolContext {
        session_id: "ses_test".to_string(),
        directory: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        cancel: None,
    }
}

#[tokio::test]
async fn edit_refuses_file_over_max_file_bytes() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("big.txt");

    // `MAX_FILE_BYTES + 1` bytes: filler followed by a replaceable final line,
    // so an uncapped edit would happily apply the change.
    let tail = b"\nneedle\n";
    let filler = usize::try_from(MAX_FILE_BYTES).unwrap() + 1 - tail.len();
    let mut before = vec![b'a'; filler];
    before.extend_from_slice(tail);
    assert_eq!(before.len(), usize::try_from(MAX_FILE_BYTES).unwrap() + 1);
    fs::write(&file, &before).unwrap();

    let result: Result<ToolResult, ToolError> = EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "big.txt", "oldString": "needle", "newString": "thread"}),
        )
        .await;

    let Err(error) = result else {
        panic!(
            "edit must refuse a {} byte file (limit {MAX_FILE_BYTES}) but it succeeded",
            MAX_FILE_BYTES + 1
        )
    };
    assert!(
        error.message.contains(&MAX_FILE_BYTES.to_string()),
        "error must name the size limit, got: {}",
        error.message
    );

    let after = fs::read(&file).unwrap();
    assert_eq!(after, before, "file must be unchanged after a refused edit");
}

#[tokio::test]
async fn patch_refuses_file_over_max_file_bytes() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("big.txt");

    let tail = b"\nneedle\n";
    let filler = usize::try_from(MAX_FILE_BYTES).unwrap() + 1 - tail.len();
    let mut before = vec![b'a'; filler];
    before.extend_from_slice(tail);
    assert_eq!(before.len(), usize::try_from(MAX_FILE_BYTES).unwrap() + 1);
    fs::write(&file, &before).unwrap();

    let patch =
        "*** Begin Patch\n*** Update File: big.txt\n@@\n needle\n-needle\n+thread\n*** End Patch\n";
    let result: Result<ToolResult, ToolError> = PatchTool
        .execute(&ctx(dir.path()), json!({"patchText": patch}))
        .await;

    let Err(error) = result else {
        panic!("patch must refuse an oversized file");
    };
    assert!(
        error.message.contains(&MAX_FILE_BYTES.to_string()),
        "error must name the size limit, got: {}",
        error.message
    );
}

#[tokio::test]
async fn patch_refuses_patch_text_over_max_file_bytes() {
    let dir = TempDir::new().unwrap();
    let oversized = "a".repeat(usize::try_from(MAX_FILE_BYTES).unwrap() + 1);
    let result: Result<ToolResult, ToolError> = PatchTool
        .execute(&ctx(dir.path()), json!({"patchText": oversized}))
        .await;

    let Err(error) = result else {
        panic!("patch must refuse oversized patch text");
    };
    assert!(
        error.message.contains(&MAX_FILE_BYTES.to_string()),
        "error must name the size limit, got: {}",
        error.message
    );
}
