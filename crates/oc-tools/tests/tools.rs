use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::TempDir;

use oc_tools::{
    CodeMapTool, EditTool, GlobTool, GrepTool, PatchTool, ReadTool, ShellTool, Tool, ToolContext,
    ToolRegistry, WriteTool,
};

fn ctx(dir: &Path) -> ToolContext {
    ToolContext {
        session_id: "ses_test".to_string(),
        directory: dir.to_path_buf(),
        cwd: dir.to_path_buf(),
        cancel: None,
    }
}

fn read(path: &Path) -> String {
    String::from_utf8(fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn registry_definitions_are_sorted_without_expectation() {
    let registry = ToolRegistry::with_defaults();
    let names: Vec<&str> = registry.names();
    assert_eq!(
        names,
        vec![
            "code_map",
            "edit",
            "glob",
            "grep",
            "patch",
            "read",
            "shell",
            "symbol_edit",
            "todowrite",
            "webfetch",
            "websearch",
            "write"
        ]
    );
    for definition in registry.definitions() {
        assert!(definition.parameters.get("properties").is_some());
        assert!(
            definition.parameters["properties"]
                .get("expectation")
                .is_none()
        );
    }
}

#[tokio::test]
async fn edit_exact_replace_all_and_unicode_tier() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("a.txt");
    fs::write(&file, "one two one").unwrap();
    let result = EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "a.txt", "oldString": "one", "newString": "X", "replaceAll": true}),
        )
        .await
        .unwrap();
    assert_eq!(read(&file), "X two X");
    assert!(result.output.unwrap().contains("2 replacements"));

    let file = dir.path().join("b.txt");
    fs::write(&file, "He said \u{201c}hello\u{201d} today").unwrap();
    EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "b.txt", "oldString": "\"hello\"", "newString": "'hi'"}),
        )
        .await
        .unwrap();
    assert_eq!(read(&file), "He said 'hi' today");
}

#[tokio::test]
async fn edit_line_trim_tier_and_crlf_remap() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("trim.txt");
    fs::write(&file, "foo   \nbar\n").unwrap();
    EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "trim.txt", "oldString": "foo    ", "newString": "baz"}),
        )
        .await
        .unwrap();
    assert_eq!(read(&file), "baz\nbar\n");

    let file = dir.path().join("crlf.txt");
    fs::write(&file, "a\r\nb\r\n").unwrap();
    EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "crlf.txt", "oldString": "a\nb", "newString": "c\nd"}),
        )
        .await
        .unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"c\r\nd\r\n");
}

#[tokio::test]
async fn edit_ambiguous_and_missing_match_errors() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("a.txt");
    fs::write(&file, "x x").unwrap();

    let error = EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "a.txt", "oldString": "x", "newString": "y"}),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("Found 2 matches"));

    let error = EditTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "a.txt", "oldString": "zzz", "newString": "y"}),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("Could not find oldString"));
}

#[tokio::test]
async fn edit_indented_tier_idempotent_retry_and_concurrent_file_locking() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("code.rs");
    fs::write(
        &file,
        "impl Server {\n    fn alpha(&self) -> i32 {\n        1\n    }\n\n    fn beta(&self) -> i32 {\n        2\n    }\n}\n",
    )
    .unwrap();

    // 1. Indentation-shifted unique block match re-indents newString to match file indentation.
    EditTool
        .execute(
            &ctx(dir.path()),
            json!({
                "path": "code.rs",
                "oldString": "fn alpha(&self) -> i32 {\n    1\n}",
                "newString": "fn alpha(&self) -> i32 {\n    10\n}"
            }),
        )
        .await
        .unwrap();
    assert!(read(&file).contains("    fn alpha(&self) -> i32 {\n        10\n    }"));

    // 2. Repeating the already-applied edit succeeds idempotently instead of failing.
    let retry = EditTool
        .execute(
            &ctx(dir.path()),
            json!({
                "path": "code.rs",
                "oldString": "    fn alpha(&self) -> i32 {\n        1\n    }",
                "newString": "    fn alpha(&self) -> i32 {\n        10\n    }"
            }),
        )
        .await
        .unwrap();
    assert!(retry.output.unwrap().contains("already present"));

    // 3. Missing oldString with a recognizable anchor line includes numbered context lines.
    let err = EditTool
        .execute(
            &ctx(dir.path()),
            json!({
                "path": "code.rs",
                "oldString": "    fn beta(&self) -> i32 {\n        999\n    }",
                "newString": "    fn beta(&self) -> i32 {\n        20\n    }"
            }),
        )
        .await
        .unwrap_err();
    assert!(err.message.contains("Could not find oldString"));
    assert!(err.message.contains("Closest matching lines in code.rs"));
    assert!(err.message.contains("fn beta(&self) -> i32"));

    // 4. Concurrent same-file edits serialize via per-file lock so neither edit is lost.
    let shared = dir.path().join("concurrent.txt");
    let lines: Vec<String> = (1..=20).map(|i| format!("line_{i} = old\n")).collect();
    let initial = lines.concat();
    fs::write(&shared, &initial).unwrap();

    let mut set = tokio::task::JoinSet::new();
    for i in 1..=20 {
        let c = ctx(dir.path());
        set.spawn(async move {
            EditTool
                .execute(
                    &c,
                    json!({
                        "path": "concurrent.txt",
                        "oldString": format!("line_{i} = old"),
                        "newString": format!("line_{i} = new")
                    }),
                )
                .await
                .unwrap();
        });
    }
    while let Some(res) = set.join_next().await {
        res.unwrap();
    }
    let final_text = read(&shared);
    for i in 1..=20 {
        assert!(
            final_text.contains(&format!("line_{i} = new\n")),
            "concurrent edit for line_{i} was lost"
        );
    }
}

#[tokio::test]
async fn write_preserves_bom_and_creates_parents() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("bom.txt");
    fs::write(&file, b"\xef\xbb\xbfold").unwrap();
    WriteTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "bom.txt", "content": "new"}),
        )
        .await
        .unwrap();
    let bytes = fs::read(&file).unwrap();
    assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
    assert_eq!(&bytes[3..], b"new");

    WriteTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "deep/nested/file.txt", "content": "hi"}),
        )
        .await
        .unwrap();
    assert_eq!(read(&dir.path().join("deep/nested/file.txt")), "hi");
}

#[tokio::test]
async fn read_pagination_binary_and_clamping() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("lines.txt");
    fs::write(&file, "1\n2\n3\n4\n5\n").unwrap();
    let result = ReadTool
        .execute(
            &ctx(dir.path()),
            json!({"path": "lines.txt", "offset": 2, "limit": 2}),
        )
        .await
        .unwrap();
    let output = result.output.unwrap();
    assert!(output.contains("Read file lines.txt, lines 2-3"));
    assert!(output.contains("2: 2\n3: 3"));
    assert!(output.contains("[Output truncated. Continue reading with offset: 4]"));
    assert_eq!(result.metadata.unwrap()["truncated"].as_bool(), Some(true));

    let binary = dir.path().join("binary.dat");
    fs::write(&binary, b"a\0b").unwrap();
    let error = ReadTool
        .execute(&ctx(dir.path()), json!({"path": "binary.dat"}))
        .await
        .unwrap_err();
    assert!(error.message.contains("Cannot read binary file"));

    let long = dir.path().join("long.txt");
    fs::write(&long, "a".repeat(2100)).unwrap();
    let output = ReadTool
        .execute(&ctx(dir.path()), json!({"path": "long.txt"}))
        .await
        .unwrap()
        .output
        .unwrap();
    assert!(output.contains("... (line truncated to 2000 chars)"));
}

#[tokio::test]
async fn read_directory_listing_is_sorted_dirs_first() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("a.txt"), "a").unwrap();
    let output = ReadTool
        .execute(&ctx(dir.path()), json!({"path": "."}))
        .await
        .unwrap()
        .output
        .unwrap();
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines[0], "Read directory ., entries 1-2");
    assert_eq!(lines[1], "sub/");
    assert_eq!(lines[2], "a.txt");
}

#[tokio::test]
async fn patch_add_update_delete_and_move() {
    let dir = TempDir::new().unwrap();
    let added = dir.path().join("new.txt");
    let result = PatchTool
        .execute(
            &ctx(dir.path()),
            json!({"patchText": "*** Begin Patch\n*** Add File: new.txt\n+hello\n+world\n*** End Patch\n"}),
        )
        .await
        .unwrap();
    assert_eq!(read(&added), "hello\nworld\n");
    assert!(result.output.unwrap().contains("A new.txt"));

    let update_target = dir.path().join("f.txt");
    fs::write(&update_target, "a\nb\nc\n").unwrap();
    PatchTool
        .execute(
            &ctx(dir.path()),
            json!({"patchText": "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n-b\n+B\n c\n*** End Patch\n"}),
        )
        .await
        .unwrap();
    assert_eq!(read(&update_target), "a\nB\nc\n");

    let move_source = dir.path().join("m.txt");
    fs::write(&move_source, "old\n").unwrap();
    PatchTool
        .execute(
            &ctx(dir.path()),
            json!({"patchText": "*** Begin Patch\n*** Update File: m.txt\n*** Move to: moved.txt\n@@\n-old\n+new\n*** End Patch\n"}),
        )
        .await
        .unwrap();
    assert!(!move_source.exists());
    assert_eq!(read(&dir.path().join("moved.txt")), "new\n");

    let deleted = dir.path().join("gone.txt");
    fs::write(&deleted, "x\n").unwrap();
    let result = PatchTool
        .execute(
            &ctx(dir.path()),
            json!({"patchText": "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n"}),
        )
        .await
        .unwrap();
    assert!(!deleted.exists());
    assert!(result.output.unwrap().contains("D gone.txt"));
}

#[tokio::test]
async fn patch_partial_failure_applies_nothing() {
    let dir = TempDir::new().unwrap();
    let result = PatchTool
        .execute(
            &ctx(dir.path()),
            json!({"patchText": "*** Begin Patch\n*** Add File: created.txt\n+x\n*** Delete File: missing.txt\n*** End Patch\n"}),
        )
        .await;
    let error = result.unwrap_err();
    assert!(error.message.contains("patch verification failed"));
    assert!(!dir.path().join("created.txt").exists());
}

#[tokio::test]
async fn glob_hidden_limit_and_gitignore() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.rs"), "a").unwrap();
    fs::write(dir.path().join("b.rs"), "b").unwrap();
    fs::write(dir.path().join("c.txt"), "c").unwrap();
    fs::write(dir.path().join(".hidden.rs"), "h").unwrap();

    let output = GlobTool
        .execute(&ctx(dir.path()), json!({"pattern": "*.rs"}))
        .await
        .unwrap();
    let text = output.output.unwrap();
    assert!(text.contains("a.rs"));
    assert!(!text.contains(".hidden.rs"));

    let output = GlobTool
        .execute(&ctx(dir.path()), json!({"pattern": "*.rs", "hidden": true}))
        .await
        .unwrap();
    assert!(output.output.unwrap().contains(".hidden.rs"));

    let output = GlobTool
        .execute(&ctx(dir.path()), json!({"pattern": "*", "limit": 1}))
        .await
        .unwrap();
    let metadata = output.metadata.unwrap();
    assert_eq!(metadata["count"].as_u64(), Some(1));
    assert_eq!(metadata["truncated"].as_bool(), Some(true));

    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    fs::write(dir.path().join("ignored.log"), "no").unwrap();
    let output = GlobTool
        .execute(&ctx(dir.path()), json!({"pattern": "*.log"}))
        .await
        .unwrap();
    assert_eq!(output.output.unwrap(), "No files found");
}

#[tokio::test]
async fn grep_literal_case_and_limit() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("a.txt");
    fs::write(&file, "a.b\naxb\n").unwrap();

    let output = GrepTool
        .execute(&ctx(dir.path()), json!({"pattern": "a.b", "literal": true}))
        .await
        .unwrap();
    let text = output.output.unwrap();
    assert!(text.contains("Found 1 matches"));
    assert!(text.contains("Line 1: a.b"));

    let file = dir.path().join("case.txt");
    fs::write(&file, "Foo\nfoo\n").unwrap();
    let output = GrepTool
        .execute(
            &ctx(dir.path()),
            json!({"pattern": "foo", "path": "case.txt"}),
        )
        .await
        .unwrap();
    assert!(output.output.unwrap().contains("Found 1 matches"));

    let output = GrepTool
        .execute(
            &ctx(dir.path()),
            json!({"pattern": "foo", "path": "case.txt", "caseSensitive": false}),
        )
        .await
        .unwrap();
    assert!(output.output.unwrap().contains("Found 2 matches"));

    let file = dir.path().join("many.txt");
    fs::write(&file, "needle\nneedle\nneedle\n").unwrap();
    let output = GrepTool
        .execute(
            &ctx(dir.path()),
            json!({"pattern": "needle", "path": "many.txt", "limit": 2}),
        )
        .await
        .unwrap();
    let metadata = output.metadata.unwrap();
    assert_eq!(metadata["matches"].as_u64(), Some(2));
    assert_eq!(metadata["truncated"].as_bool(), Some(true));
}

#[tokio::test]
async fn shell_reports_exit_code() {
    let dir = TempDir::new().unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "exit 3", "timeout": 30_000}),
        )
        .await
        .unwrap();
    let metadata = result.metadata.unwrap();
    assert_eq!(metadata["exit"].as_i64(), Some(3));
    assert!(result.output.unwrap().contains("Exited with code 3"));
    assert!(metadata["outputPath"].as_str().is_some());
}

#[tokio::test]
async fn shell_timeout_kills_the_command() {
    let dir = TempDir::new().unwrap();
    #[cfg(unix)]
    let command = "sleep 5";
    #[cfg(windows)]
    let command = "ping -n 6 127.0.0.1 > nul";

    let started = Instant::now();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": command, "timeout": 200}),
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    let metadata = result.metadata.unwrap();
    assert_eq!(metadata["timeout"].as_bool(), Some(true));
    assert!(
        result
            .output
            .unwrap()
            .contains("Command exceeded timeout of 200 ms")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_exports_session_environment() {
    let dir = TempDir::new().unwrap();
    let result = ShellTool
        .execute(
            &ctx(dir.path()),
            json!({"command": "printf '%s' \"$OPENCODE_SESSION_ID\""}),
        )
        .await
        .unwrap();
    assert_eq!(result.output.unwrap().trim(), "ses_test");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_reaps_background_children_in_process_group_on_exit() {
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("bg.pid");
    let cmd = format!(
        "sleep 60 >/dev/null 2>&1 & echo $! > {}; echo done",
        pid_file.display()
    );
    let result = ShellTool
        .execute(&ctx(dir.path()), json!({"command": cmd}))
        .await
        .unwrap();
    assert_eq!(result.output.unwrap().trim(), "done");
    let bg_pid: u32 = read(&pid_file).trim().parse().unwrap();
    // Process group cleanup should have terminated the background sleep process.
    // A killed child stays visible to `kill -0` until its parent of last resort
    // reaps it, so poll instead of sampling once: a single non-blocking check
    // races with that reaping and fails intermittently.
    let gone = |pid: u32| {
        !std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !gone(bg_pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        gone(bg_pid),
        "background child pid {bg_pid} should be reaped when ShellTool completes"
    );
}

#[tokio::test]
async fn code_map_outline_and_symbol_operations() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("server.rs");
    fs::write(
        &file,
        r#"
/// HTTP Server supervisor.
pub struct HttpServer {
    port: u16,
}

impl HttpServer {
    pub async fn start(&self) {
        println!("listening");
    }
}

fn internal_worker() {
    println!("work");
}
"#,
    )
    .unwrap();

    let tool = CodeMapTool;

    // Test outline
    let outline_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "outline",
                "path": "server.rs"
            }),
        )
        .await
        .unwrap();
    let outline_text = outline_res.output.unwrap();
    assert!(outline_text.contains("HttpServer"));
    assert!(outline_text.contains("pub struct HttpServer"));
    assert!(outline_text.contains("pub async fn start"));
    assert!(outline_text.contains("fn internal_worker"));

    // Test symbol search
    let sym_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "symbol",
                "query": "HttpServer"
            }),
        )
        .await
        .unwrap();
    let sym_text = sym_res.output.unwrap();
    assert!(sym_text.contains("server.rs"));
    assert!(sym_text.contains("[struct]"));

    // Test references search
    let ref_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "references",
                "query": "listening"
            }),
        )
        .await
        .unwrap();
    let ref_text = ref_res.output.unwrap();
    assert!(ref_text.contains("server.rs"));
    assert!(ref_text.contains("listening"));

    // Test map operation
    let map_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "map",
                "query": "server start"
            }),
        )
        .await
        .unwrap();
    let map_text = map_res.output.unwrap();
    assert!(map_text.contains("<code_map") || map_text.contains("<ctx"));
    assert!(map_text.contains("server.rs"));
    assert!(map_text.contains("start"));
}

#[tokio::test]
async fn code_map_native_directory_outline_and_search() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("src");
    fs::create_dir_all(&sub).unwrap();
    fs::write(
        sub.join("client.ts"),
        r"
export interface ClientOptions { timeout: number; }
export class ApiClient {
    async connect(): Promise<void> {}
}
",
    )
    .unwrap();
    fs::write(
        sub.join("app.py"),
        r"
class AppController:
    def bootstrap(self):
        pass
",
    )
    .unwrap();

    let tool = CodeMapTool;

    // Test directory outline across languages
    let outline_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "outline",
                "path": "src"
            }),
        )
        .await
        .unwrap();
    let outline_text = outline_res.output.unwrap();
    assert!(outline_text.contains("client.ts"));
    assert!(outline_text.contains("ClientOptions"));
    assert!(outline_text.contains("ApiClient"));
    assert!(outline_text.contains("app.py"));
    assert!(outline_text.contains("AppController"));

    // Test symbol search for TS interface
    let sym_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "symbol",
                "query": "ClientOptions"
            }),
        )
        .await
        .unwrap();
    let sym_text = sym_res.output.unwrap();
    assert!(sym_text.contains("client.ts"));
    assert!(sym_text.contains("[interface]"));

    // Test symbol search for Python method
    let py_sym_res = tool
        .execute(
            &ctx(dir.path()),
            json!({
                "operation": "symbol",
                "query": "bootstrap"
            }),
        )
        .await
        .unwrap();
    let py_sym_text = py_sym_res.output.unwrap();
    assert!(py_sym_text.contains("app.py"));
    assert!(py_sym_text.contains("[method]"));
}
