//! Project, VCS and truthful-list routes through the real router.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use axum::http::{Method, Request, StatusCode};
use oc_server::{AppState, router};
use oc_store::Store;
use oc_types::{ServerInfo, ServerPaths};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    _db: tempfile::TempDir,
    directory: tempfile::TempDir,
    state: AppState,
}

async fn harness() -> Harness {
    let db = tempfile::tempdir().unwrap();
    let store = Store::open(db.path().join("data.sqlite")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let state = state_for(&directory, store);
    Harness {
        _db: db,
        directory,
        state,
    }
}

fn state_for(directory: &tempfile::TempDir, store: Store) -> AppState {
    AppState::new(
        ServerInfo {
            version: "0.0.0-test".to_owned(),
            pid: 4242,
            hostname: Some("127.0.0.1".to_owned()),
            urls: vec!["http://127.0.0.1:4096".to_owned()],
            paths: ServerPaths {
                tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            },
            capabilities: None,
        },
        None,
        directory.path().to_path_buf(),
    )
    .with_store(Arc::new(store))
}

/// Send one request, optionally with a directory header.
async fn call(
    app: &axum::Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    directory: Option<&Path>,
) -> (StatusCode, Option<Value>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(directory) = directory {
        builder = builder.header(
            HeaderName::from_static("x-opencode-directory"),
            HeaderValue::from_str(&directory.to_string_lossy()).unwrap(),
        );
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let value = if bytes.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&bytes).unwrap())
    };
    (status, value)
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?} failed to spawn: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Initialize a repository with one commit containing `a.txt`.
fn init_repository(directory: &Path) {
    git(directory, &["init", "-q"]);
    git(directory, &["config", "user.email", "test@example.invalid"]);
    git(directory, &["config", "user.name", "Test"]);
    std::fs::write(directory.join("a.txt"), "one\ntwo\n").unwrap();
    git(directory, &["add", "."]);
    git(directory, &["commit", "-qm", "init"]);
}

#[tokio::test]
async fn project_list_patch_and_archive() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/session",
        Some(json!({"title": "project owner"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    let (status, body) = call(&app, Method::GET, "/api/project", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let projects = body.unwrap();
    let projects = projects.as_array().expect("bare array");
    assert_eq!(projects.len(), 1, "{projects:?}");
    let project = &projects[0];
    let id = project["id"].as_str().unwrap();
    assert!(id.starts_with("prj_"), "{project:?}");
    assert_eq!(
        project["canonical"],
        json!(harness.directory.path().to_string_lossy())
    );
    assert!(project["time"]["created"].is_i64());
    assert!(project["time"]["updated"].is_i64());
    assert!(project["time"]["active"].is_i64());
    assert_eq!(project["sandboxes"], json!([]));
    assert!(project.get("name").is_none());
    // No repository marker in the location, so no `vcs`.
    assert!(project.get("vcs").is_none());

    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/api/project/{id}"),
        Some(json!({
            "name": "Renamed",
            "icon": {"url": "https://example.invalid/icon.png"},
            "archived": false,
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let updated = body.unwrap();
    assert_eq!(updated["name"], "Renamed");
    assert_eq!(updated["icon"]["url"], "https://example.invalid/icon.png");
    assert_eq!(updated["id"], json!(id));

    // Unsupported upstream fields are reported, not silently dropped.
    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/api/project/{id}"),
        Some(json!({"canonical": "/somewhere/else"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "BadRequestError");

    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/api/project/{id}"),
        Some(json!({"archived": true})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let archived = body.unwrap();
    assert!(archived["time"]["archived"].is_i64(), "{archived:?}");

    let (_, body) = call(&app, Method::GET, "/api/project", None, None).await;
    assert_eq!(body.unwrap(), json!([]));
    let (_, body) = call(&app, Method::GET, "/api/project?archived=true", None, None).await;
    assert_eq!(body.unwrap().as_array().unwrap().len(), 1);

    let (status, body) = call(
        &app,
        Method::PATCH,
        "/api/project/prj_unknown",
        Some(json!({"name": "x"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "ProjectNotFoundError");

    let (status, body) = call(&app, Method::GET, "/api/project?archived=yes", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "SchemaError");
}

#[tokio::test]
async fn project_reports_vcs_marker() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    init_repository(harness.directory.path());
    let app = router(harness.state.clone());

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/session",
        Some(json!({"title": "git project"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(&app, Method::GET, "/api/project", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()[0]["vcs"], "git");
}

#[tokio::test]
async fn vcs_info_init_and_non_repository() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let directory = harness.directory.path();
    let app = router(harness.state.clone());

    // No repository: info says so without erroring.
    let (status, body) = call(&app, Method::GET, "/api/vcs", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    assert_eq!(body["data"]["branch"], json!({}));
    assert!(body["data"].get("provider").is_none());

    let (status, body) = call(&app, Method::GET, "/api/vcs/status", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"], json!([]));
    let (status, body) = call(&app, Method::GET, "/api/vcs/base", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"], Value::Null);

    let (status, _) = call(&app, Method::POST, "/api/vcs/init", None, Some(directory)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(&app, Method::POST, "/api/vcs/init", None, Some(directory)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert_eq!(body.unwrap()["_tag"], "ConflictError");

    let (status, body) = call(&app, Method::GET, "/api/vcs", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert_eq!(body["data"]["provider"], "git");
    let current = git(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    assert_eq!(body["data"]["branch"]["current"], json!(current));
    // An unborn branch has no ref yet, so there is no default branch to report.
    assert!(body["data"]["branch"].get("default").is_none());

    // An unknown provider is rejected before touching the filesystem.
    let other = tempfile::tempdir().unwrap();
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/vcs/init?provider=hg",
        None,
        Some(other.path()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "InvalidRequestError");
}

#[tokio::test]
async fn vcs_status_branch_and_diff() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let directory = harness.directory.path();
    init_repository(directory);
    let app = router(harness.state.clone());

    let current = git(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"]);

    let (status, body) = call(&app, Method::GET, "/api/vcs/branch", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"], json!([current]));

    // Clean working copy.
    let (status, body) = call(&app, Method::GET, "/api/vcs/status", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"], json!([]));

    // The review base on the default branch is the default itself.
    let (status, body) = call(&app, Method::GET, "/api/vcs/base", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    let base = body.unwrap()["data"].clone();
    assert_eq!(base["name"], json!(current));
    assert_eq!(base["source"], "default");
    assert_eq!(base["ref"], json!(format!("refs/heads/{current}")));

    // One modified tracked file and two untracked files (one nested).
    std::fs::write(directory.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(directory.join("c.txt"), "new\n").unwrap();
    std::fs::create_dir_all(directory.join("d")).unwrap();
    std::fs::write(directory.join("d/e.txt"), "q\n").unwrap();

    let (status, body) = call(&app, Method::GET, "/api/vcs/status", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body.unwrap()["data"].clone();
    assert_eq!(
        entries,
        json!([
            {"file": "a.txt", "additions": 1, "deletions": 0, "status": "modified"},
            {"file": "c.txt", "additions": 1, "deletions": 0, "status": "added"},
            {"file": "d/e.txt", "additions": 1, "deletions": 0, "status": "added"},
        ])
    );

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=working",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rows = body.unwrap()["data"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0]["file"], "a.txt");
    assert_eq!(rows[0]["status"], "modified");
    assert!(
        rows[0]["patch"].as_str().unwrap().contains("+three"),
        "{rows:?}"
    );
    assert_eq!(rows[1]["file"], "c.txt");
    assert_eq!(rows[1]["status"], "added");
    assert!(
        rows[1]["patch"].as_str().unwrap().contains("+new"),
        "{rows:?}"
    );
    assert_eq!(rows[2]["file"], "d/e.txt");

    // Folder mode collapses the untracked directory into one `dir/` entry.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=working&untracked=folders&context=1",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rows = body.unwrap()["data"].clone();
    let rows = rows.as_array().unwrap();
    assert!(rows.iter().any(|row| row["file"] == "d/"), "{rows:?}");
    assert!(!rows.iter().any(|row| row["file"] == "d/e.txt"), "{rows:?}");

    // Nothing committed since HEAD.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=committed",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"], json!([]));

    // Invalid mode is a schema error.
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=bogus",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "SchemaError");
}

#[tokio::test]
async fn vcs_diff_large_output_does_not_deadlock() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let directory = harness.directory.path();
    git(directory, &["init", "-q"]);
    git(directory, &["config", "user.name", "Test"]);
    git(directory, &["config", "user.email", "test@test.local"]);

    std::fs::write(directory.join("large.txt"), "initial\n").unwrap();
    git(directory, &["add", "large.txt"]);
    git(directory, &["commit", "-m", "init", "-q"]);

    // 2 MB of content to overflow the 1 MB diff cap and 64 KB OS pipe buffer
    let line = "a".repeat(100) + "\n";
    let mut content = String::with_capacity(20_000 * 101);
    for _ in 0..20_000 {
        content.push_str(&line);
    }
    std::fs::write(directory.join("large.txt"), &content).unwrap();

    let app = router(harness.state.clone());
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=working",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rows = body.unwrap()["data"].clone();
    assert!(rows.is_array());

    // Also verify large UNTRACKED file (> 256 KB MAX_FILE_PATCH_BYTES) does not fail
    std::fs::write(directory.join("untracked_large.txt"), &content).unwrap();
    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=working",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rows = body.unwrap()["data"].clone();
    assert!(rows.is_array());
}

#[tokio::test]
async fn vcs_diff_before_first_commit() {
    if !git_available() {
        return;
    }
    let harness = harness().await;
    let directory = harness.directory.path();
    git(directory, &["init", "-q"]);
    std::fs::write(directory.join("a.txt"), "hello\n").unwrap();
    let app = router(harness.state.clone());

    let (status, body) = call(&app, Method::GET, "/api/vcs/base", None, Some(directory)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["data"], Value::Null);

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/vcs/diff?mode=working",
        None,
        Some(directory),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let rows = body.unwrap()["data"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["file"], "a.txt");
    assert_eq!(rows[0]["status"], "added");
    assert_eq!(rows[0]["additions"], 1);
    assert!(
        rows[0]["patch"].as_str().unwrap().contains("+hello"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn truthful_list_routes() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    for route in [
        "/api/form",
        "/api/shell",
        "/api/integration",
        "/api/websearch/provider",
    ] {
        let (status, body) = call(&app, Method::GET, route, None, None).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        let body = body.unwrap();
        assert!(
            body["location"]["directory"].is_string(),
            "{route}: {body:?}"
        );
        assert_eq!(body["data"], json!([]), "{route}");
    }

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/experimental/usage/limits",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body = body.unwrap();
    assert!(body["location"]["directory"].is_string());
    assert_eq!(body["data"], Value::Null);

    let (status, body) = call(&app, Method::GET, "/api/credential", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!({"data": []}));

    let (status, body) = call(&app, Method::GET, "/api/mcp", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body = body.unwrap();
    assert_eq!(body["_tag"], "ServiceUnavailableError");
    assert_eq!(body["message"], "MCP manager not attached");

    let (status, _) = call(&app, Method::GET, "/api/reference", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn worktree_list_is_bare_and_checked() {
    let harness = harness().await;
    let app = router(harness.state.clone());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/session",
        Some(json!({"title": "worktree owner"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let project_id = body.unwrap()["data"]["projectID"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, body) = call(
        &app,
        Method::GET,
        &format!("/api/worktree?projectID={project_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap(), json!([]));

    let (status, body) = call(&app, Method::GET, "/api/worktree", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["_tag"], "SchemaError");

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/worktree?projectID=prj_unknown",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["_tag"], "ProjectNotFoundError");
}
